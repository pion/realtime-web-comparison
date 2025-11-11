// str0m test server with a single tokio::select! loop (WS, UDP, timer)
// should keeps the WebSocket/TLS flow unchanged as other clients. tested on Windows & Linux by.
// doesn't work on firefox on windows (dtls handshake fails).
use bytes::Bytes;
use crossbeam_channel as cb;
use futures_util::{SinkExt, StreamExt};
use http_body_util::Full;
use hyper::{
    body::Incoming,
    header::{CONNECTION, SEC_WEBSOCKET_ACCEPT, SEC_WEBSOCKET_KEY, UPGRADE},
    http::StatusCode,
    server::conn::http1,
    service::service_fn,
    upgrade::Upgraded,
    Method, Request, Response,
};
use hyper_util::rt::TokioIo;
use if_addrs::get_if_addrs;
use mimalloc::MiMalloc;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use serde::{Deserialize, Serialize};
use socket2::SockRef;
use std::error::Error;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, UdpSocket};
use tokio::time::{sleep_until, Instant as TokioInstant};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::{
    tungstenite::{handshake::derive_accept_key, protocol::Role, Message},
    WebSocketStream,
};

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

const TLS_BIND_IP: &str = "127.0.0.1";
const TLS_BIND_PORT: u16 = 8002;
const REDIRECT_HOST: &str = "localhost";

// Platform-specific configuration
struct PlatformConfig {
    initial_batch_size: usize,
    high_water_bytes: usize,
    low_water_bytes: usize,
    max_outputs_per_call: usize,
    success_batches_for_growth: usize,
    max_batch_size: usize,
    min_batch_size: usize,
    check_backpressure_during_write: bool,
    check_backpressure_every_n_writes: usize,
}

impl PlatformConfig {
    fn current() -> Self {
        #[cfg(windows)]
        {
            Self {
                initial_batch_size: 3,
                high_water_bytes: 2 * 1024 * 1024, // 2 MiB
                low_water_bytes: 1 * 1024 * 1024,  // 1 MiB
                max_outputs_per_call: 10,
                success_batches_for_growth: 10,
                max_batch_size: 10,
                min_batch_size: 1,
                check_backpressure_during_write: true,
                check_backpressure_every_n_writes: 1,
            }
        }
        #[cfg(not(windows))]
        {
            Self {
                initial_batch_size: 20,
                high_water_bytes: 10 * 1024 * 1024, // 10 MiB
                low_water_bytes: 5 * 1024 * 1024,   // 5 MiB
                max_outputs_per_call: 50,
                success_batches_for_growth: 5,
                max_batch_size: 100,
                min_batch_size: 10,
                check_backpressure_during_write: false,
                check_backpressure_every_n_writes: 0,
            }
        }
    }

    /// Determine if this platform uses conservative (linear) batch growth
    fn uses_linear_growth(&self) -> bool {
        #[cfg(windows)]
        {
            true
        }
        #[cfg(not(windows))]
        {
            false
        }
    }
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type")]
enum WsInbound {
    #[serde(rename = "offer")]
    Offer { sdp: String },
    #[serde(rename = "candidate")]
    Candidate {
        candidate: String,
        #[serde(rename = "sdpMid")]
        #[allow(dead_code)]
        sdp_mid: Option<String>,
        #[serde(rename = "sdpMLineIndex")]
        #[allow(dead_code)]
        sdp_m_line_index: Option<u32>,
    },
    #[serde(rename = "endOfCandidates")]
    EndOfCandidates {},
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
#[allow(dead_code)]
enum WsOutbound {
    #[serde(rename = "answer")]
    Answer { sdp: String },
    #[serde(rename = "candidate")]
    Candidate {
        candidate: String,
        #[serde(rename = "sdpMid")]
        sdp_mid: Option<String>,
        #[serde(rename = "sdpMLineIndex")]
        sdp_m_line_index: Option<u32>,
    },
    #[serde(rename = "endOfCandidates")]
    EndOfCandidates {},
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rustls::crypto::CryptoProvider::install_default(rustls::crypto::ring::default_provider())
        .map_err(|_| anyhow::anyhow!("Failed to install rustls ring provider"))?;

    let cert_path = Path::new("../../certs/localhost.pem");
    let key_path = Path::new("../../certs/localhost-key.pem");
    let cert_pem = std::fs::read(cert_path)?;
    let key_pem = std::fs::read(key_path)?;
    let cert = rustls_pemfile::certs(&mut &cert_pem[..])
        .next()
        .ok_or_else(|| anyhow::anyhow!("no certificate found"))?
        .map(CertificateDer::from)?;
    let key = rustls_pemfile::pkcs8_private_keys(&mut &key_pem[..])
        .next()
        .ok_or_else(|| anyhow::anyhow!("no private key found"))?
        .map(PrivateKeyDer::from)?;
    let rustls_cfg = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)?;
    let acceptor = TlsAcceptor::from(Arc::new(rustls_cfg));

    let bind_addr = format!("{TLS_BIND_IP}:{TLS_BIND_PORT}");
    let listener = TcpListener::bind(&bind_addr).await?;
    eprintln!("Listening on wss://{bind_addr}");

    loop {
        let (tcp, _) = listener.accept().await?;
        let acceptor = acceptor.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_client(acceptor, tcp).await {
                eprintln!("Client error: {e:?}");
            }
        });
    }
}

async fn handle_client(acceptor: TlsAcceptor, tcp: tokio::net::TcpStream) -> anyhow::Result<()> {
    // detect http and redirect to https
    let mut peek_buf = [0u8; 1];
    if let Ok(n) = tcp.peek(&mut peek_buf).await {
        if n > 0 && peek_buf[0].is_ascii_alphabetic() {
            respond_with_http_redirect(tcp).await?;
            return Ok(());
        }
    }

    let tls = acceptor.accept(tcp).await?;
    let io = TokioIo::new(tls);
    http1::Builder::new()
        .serve_connection(io, service_fn(handle_http))
        .with_upgrades()
        .await?;
    Ok(())
}

async fn respond_with_http_redirect(mut tcp: tokio::net::TcpStream) -> anyhow::Result<()> {
    const BODY: &str = "Redirecting to HTTPS\r\n";
    let response = format!(
        "HTTP/1.1 302 Found\r\n\
         Location: https://{host}:{port}/\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {length}\r\n\
         Connection: close\r\n\
         \r\n",
        host = REDIRECT_HOST,
        port = TLS_BIND_PORT,
        length = BODY.len()
    );

    tcp.write_all(response.as_bytes()).await?;
    tcp.write_all(BODY.as_bytes()).await?;
    tcp.shutdown().await?;
    Ok(())
}
async fn handle_http(req: Request<Incoming>) -> Result<Response<Full<Bytes>>, hyper::Error> {
    if !is_websocket_upgrade(&req) {
        let body = Full::new(Bytes::from_static(
            b"WebRTC Signaling Server - Use WebSocket connection for signaling",
        ));
        let response = Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/plain; charset=utf-8")
            .header("Connection", "close")
            .body(body)
            .unwrap();
        return Ok(response);
    }

    let key = match req.headers().get(SEC_WEBSOCKET_KEY) {
        Some(k) => k.clone(),
        None => {
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Full::new(Bytes::from_static(
                    b"Missing Sec-WebSocket-Key header",
                )))
                .unwrap());
        }
    };

    let accept_key = derive_accept_key(key.as_bytes());

    tokio::spawn(async move {
        match hyper::upgrade::on(req).await {
            Ok(upgraded) => {
                let upgraded = TokioIo::new(upgraded);
                let ws = WebSocketStream::from_raw_socket(upgraded, Role::Server, None).await;
                if let Err(err) = handle_signaling(ws).await {
                    eprintln!("Client error: {err:?}");
                }
            }
            Err(err) => eprintln!("WebSocket upgrade error: {err:?}"),
        }
    });

    Ok(Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(CONNECTION, "Upgrade")
        .header(UPGRADE, "websocket")
        .header(SEC_WEBSOCKET_ACCEPT, accept_key)
        .body(Full::new(Bytes::new()))
        .unwrap())
}

fn is_websocket_upgrade(req: &Request<Incoming>) -> bool {
    if req.method() != Method::GET {
        return false;
    }

    connection_header_contains_upgrade(req)
        && req
            .headers()
            .get(UPGRADE)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.eq_ignore_ascii_case("websocket"))
            .unwrap_or(false)
        && req.headers().contains_key(SEC_WEBSOCKET_KEY)
}

fn connection_header_contains_upgrade(req: &Request<Incoming>) -> bool {
    req.headers()
        .get(CONNECTION)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
        })
        .unwrap_or(false)
}

// This function handles one WebRTC connection from start to finish after the WebSocket upgrade.
async fn handle_signaling(mut ws: WebSocketStream<TokioIo<Upgraded>>) -> anyhow::Result<()> {
    let first_msg = ws
        .next()
        .await
        .ok_or_else(|| anyhow::anyhow!("client closed"))??;
    let offer = match first_msg {
        Message::Text(t) => serde_json::from_str::<WsInbound>(&t)?,
        Message::Binary(b) => serde_json::from_slice::<WsInbound>(&b)?,
        _ => anyhow::bail!("unexpected first WS message"),
    };
    let offer_sdp = match offer {
        WsInbound::Offer { sdp } => sdp,
        _ => anyhow::bail!("first message must be an offer"),
    };

    let bind_ip = pick_bind_v4_for_offer(&offer_sdp);

    let udp = UdpSocket::bind(SocketAddr::new(IpAddr::V4(bind_ip), 0)).await?;
    let local_udp = udp.local_addr()?;
    eprintln!("UDP bound to: {local_udp}");

    let std_udp = udp.into_std()?;

    SockRef::from(&std_udp).set_send_buffer_size(20 * 1024 * 1024)?;
    configure_udp_socket_platform_specific(&std_udp)?;
    let got = SockRef::from(&std_udp).send_buffer_size()?;
    eprintln!("SO_SNDBUF set; effective size = {} bytes", got);

    let std_udp_arc = Arc::new(std_udp);

    let (ws_to_rtc_tx, ws_to_rtc_rx) = cb::unbounded::<WsInbound>();
    let (udp_to_rtc_tx, udp_to_rtc_rx) = cb::unbounded::<UdpIn>();
    let (timeout_tx, timeout_rx) = cb::unbounded::<()>();
    let (next_to_async_tx, next_to_async_rx) = cb::unbounded::<Instant>();
    let (answer_tx, answer_rx) = cb::unbounded::<String>();

    let std_udp_for_rtc = Arc::clone(&std_udp_arc);
    let offer_for_rtc = offer_sdp.clone();
    tokio::task::spawn_blocking(move || {
        let _ = run_rtc_thread(
            offer_for_rtc,
            IpAddr::V4(bind_ip),
            local_udp.port(),
            ws_to_rtc_rx,
            udp_to_rtc_rx,
            timeout_rx,
            next_to_async_tx,
            answer_tx,
            std_udp_for_rtc,
        );
    });

    let answer = tokio::task::spawn_blocking(move || answer_rx.recv()).await??;

    let answer_obj = serde_json::json!({ "type": "answer", "sdp": answer });
    ws.send(Message::Text(serde_json::to_string(&answer_obj)?.into()))
        .await?;

    let udp_recv = {
        let cloned = std_udp_arc.as_ref().try_clone()?;
        tokio::net::UdpSocket::from_std(cloned)?
    };

    let mut buf = vec![0u8; 65507]; // Maximum UDP packet size for better throughput
    let local_addr_for_recv = SocketAddr::new(IpAddr::V4(bind_ip), local_udp.port());
    let mut next_deadline = Instant::now() + Duration::from_millis(50);

    loop {
        if let Ok(t) = next_to_async_rx.try_recv() {
            next_deadline = t;
        }

        let mut ws_next = ws.next();
        let sleep_fut = sleep_until(TokioInstant::from_std(next_deadline));
        tokio::pin!(sleep_fut);

        tokio::select! {
            // ws candidates
            maybe_msg = &mut ws_next => {
                match maybe_msg {
                    Some(Ok(Message::Text(t))) => {
                        if let Ok(inb) = serde_json::from_str::<WsInbound>(&t) { let _ = ws_to_rtc_tx.send(inb); }
                    }
                    Some(Ok(Message::Binary(b))) => {
                        if let Ok(inb) = serde_json::from_slice::<WsInbound>(&b) { let _ = ws_to_rtc_tx.send(inb); }
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => { break; }
                    _ => {}
                }
            }

            // Handle incoming UDP packets (STUN, DTLS, media data)
            recv = udp_recv.recv_from(&mut buf) => {
                match recv {
                    Ok((n, src)) => {
                        if n > 0 {
                            let data = buf[..n].to_vec();
                            let _ = udp_to_rtc_tx.send(UdpIn { buf: data, src, dst: local_addr_for_recv });
                        }
                    }
                    Err(e) => {
                        if is_transient_udp_recv_err(&e) { /* ignore */ }
                        else { eprintln!("UDP receive error (continuing): {e}"); }
                    }
                }
            }

            // Timer tick - tell str0m to check for timeouts
            _ = &mut sleep_fut => { let _ = timeout_tx.send(()); }
        }
    }

    Ok(())
}

// Simple struct to hold information about incoming UDP packets
#[derive(Debug)]
struct UdpIn {
    buf: Vec<u8>,
    src: SocketAddr,
    dst: SocketAddr,
}

// configure UDP socket with platform-specific settings
fn configure_udp_socket_platform_specific(_udp_socket: &std::net::UdpSocket) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        use winapi::shared::minwindef::{DWORD, LPVOID};
        use winapi::um::winsock2::{setsockopt, SO_SNDBUF, SOL_SOCKET, SOCKET_ERROR, WSAGetLastError, WSAIoctl};

        // SIO_UDP_CONNRESET = _WSAIOW(IOC_VENDOR, 12) = 0x9800000C
        const SIO_UDP_CONNRESET: DWORD = 0x9800000C;

        let socket = _udp_socket.as_raw_socket();
        let buffer_size: i32 = 40 * 1024 * 1024;
        let result = unsafe {
            setsockopt(
                socket as _,
                SOL_SOCKET,
                SO_SNDBUF,
                &buffer_size as *const _ as *const _,
                std::mem::size_of::<i32>() as _,
            )
        };
        if result == SOCKET_ERROR {
            let err = unsafe { WSAGetLastError() };
            eprintln!("Warning: Failed to set UDP send buffer size, error: {}", err);
        } else {
            eprintln!("Set UDP send buffer to {} bytes", buffer_size);
        }

        // disable spurious UDP connection reset errors on Windows
        let mut bytes_returned: DWORD = 0;
        let mut enable: u32 = 0;

        let rc = unsafe {
            WSAIoctl(
                socket as _,
                SIO_UDP_CONNRESET,
                &mut enable as *mut _ as LPVOID,
                std::mem::size_of::<u32>() as DWORD,
                std::ptr::null_mut(),
                0,
                &mut bytes_returned as *mut _,
                std::ptr::null_mut(),
                None,
            )
        };
        if rc == SOCKET_ERROR {
            eprintln!("Warning: failed to set SIO_UDP_CONNRESET");
        } else {
            eprintln!("Disabled UDP connection reset errors (SIO_UDP_CONNRESET)");
        }
    }
    Ok(())
}

// Check if a UDP receive error is something we can safely ignore
// Windows has some quirks with UDP error handling
fn is_transient_udp_recv_err(_e: &std::io::Error) -> bool {
    #[cfg(windows)]
    {
        _e.kind() == std::io::ErrorKind::ConnectionReset || _e.raw_os_error() == Some(10054)
    }
    #[cfg(not(windows))]
    {
        false
    }
}

// Check if this is the weird EINVAL error that happens on Linux
// when we try to send to a non-loopback address from a loopback-bound socket
fn is_send_einval_to_non_loopback(e: &std::io::Error, dest: SocketAddr) -> bool {
    if e.raw_os_error() == Some(22) {
        if let IpAddr::V4(v4) = dest.ip() {
            return !v4.is_loopback();
        }
    }
    false
}

// Extract IPv4 addresses from the browser's SDP offer
// We look for "a=candidate" lines and pull out the IP addresses
// This helps us decide which network interface to bind to
fn extract_offer_v4s(sdp: &str) -> Vec<Ipv4Addr> {
    let mut ips = Vec::new();
    for line in sdp.lines() {
        // SDP candidate lines look like: a=candidate:1 1 UDP 12345 192.168.1.100 12345 typ host
        if let Some(rest) = line.strip_prefix("a=candidate:") {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if parts.len() >= 6 {
                // Skip mDNS hostnames (end with .local)
                if !parts[4].ends_with(".local") {
                    if let Ok(IpAddr::V4(v4)) = parts[4].parse::<IpAddr>() {
                        ips.push(v4);
                    }
                }
            }
        }
    }
    ips
}

// Check how many bits two IP addresses share in common
// This helps us find the best matching network interface
fn same_lan_prefix(a: Ipv4Addr, b: Ipv4Addr) -> u32 {
    let ao = a.octets();
    let bo = b.octets();
    if ao == bo {
        return 32; // Exact match
    }
    if ao[0] == bo[0] && ao[1] == bo[1] && ao[2] == bo[2] {
        return 24; // Same /24 subnet
    }
    if ao[0] == bo[0] && ao[1] == bo[1] {
        return 16; // Same /16 subnet
    }
    if ao[0] == bo[0] {
        return 8;  // Same /8 subnet
    }
    0  // No common prefix
}

// Check if a network interface name suggests it's a virtual/tunnel interface
// We prefer real network interfaces for WebRTC connections
fn if_is_virtual(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    [
        "virtualbox",
        "vmware",
        "hyper-v",
        "wintun",
        "tailscale",
        "zerotier",
        "docker",
        "vbox",
        "bridge",
        "hamachi",
        "loopback",
        "veth",
    ]
    .iter()
    .any(|k| n.contains(k))
}

fn pick_bind_v4_for_offer(offer_sdp: &str) -> Ipv4Addr {
    if let Ok(s) = std::env::var("BIND_UDP_IP") {
        if let Ok(ip) = s.parse::<Ipv4Addr>() {
            return ip;
        }
    }

    let offer_ips = extract_offer_v4s(offer_sdp);
    if offer_ips.iter().any(|ip| ip.is_loopback()) {
        return Ipv4Addr::LOCALHOST;
    }

    let mut best: Option<(u32, Ipv4Addr)> = None;
    if let Ok(ifs) = get_if_addrs() {
        for i in &ifs {
            if let IpAddr::V4(ip) = i.ip() {
                if i.is_loopback() || ip.is_loopback() {
                    continue;
                }
                if if_is_virtual(&i.name) {
                    continue;
                }
                let o = ip.octets();
                let is_rfc1918 = (o[0] == 10)
                    || (o[0] == 172 && (16..=31).contains(&o[1]))
                    || (o[0] == 192 && o[1] == 168);
                if !is_rfc1918 {
                    continue;
                }
                let mut rank = 0;
                for off in &offer_ips {
                    rank = rank.max(same_lan_prefix(ip, *off));
                }
                if best.as_ref().map_or(true, |(r, _)| rank > *r) {
                    best = Some((rank, ip));
                }
            }
        }
        if let Some((_, ip)) = best {
            return ip;
        }

        for i in ifs {
            if let IpAddr::V4(ip) = i.ip() {
                if i.is_loopback() || ip.is_loopback() {
                    continue;
                }
                if if_is_virtual(&i.name) {
                    continue;
                }
                let o = ip.octets();
                let is_rfc1918 = (o[0] == 10)
                    || (o[0] == 172 && (16..=31).contains(&o[1]))
                    || (o[0] == 192 && o[1] == 168);
                if is_rfc1918 {
                    return ip;
                }
            }
        }
    }

    Ipv4Addr::LOCALHOST
}

// Build RTC configuration with platform-specific settings
fn build_rtc_config() -> anyhow::Result<str0m::Rtc> {
    #[cfg(windows)]
    {
        let cert_opts = str0m::config::DtlsCertOptions {
            pkey_type: str0m::config::DtlsPKeyType::Rsa2048,
            ..Default::default()
        };
        let crypto_provider = str0m::config::CryptoProvider::WinCrypto;
        let dtls_cert = str0m::config::DtlsCert::new(crypto_provider, cert_opts);
        let str0m_config = str0m::RtcConfig::new()
            .set_dtls_cert_config(str0m::DtlsCertConfig::PregeneratedCert(dtls_cert));
        Ok(str0m_config.build())
    }
    #[cfg(not(windows))]
    {
        Ok(str0m::RtcConfig::new().build())
    }
}

// Platform-specific sleep strategy
fn platform_sleep(_did_work: bool) {
    #[cfg(windows)]
    {
        // Keep normal sleep time even when sending to avoid overwhelming Windows
        std::thread::sleep(Duration::from_millis(1));
    }
    #[cfg(not(windows))]
    {
        if !_did_work {
            std::thread::yield_now();
        }
    }
}

fn run_rtc_thread(
    offer_sdp: String,
    local_ip: IpAddr,
    udp_port: u16,
    ws_in: cb::Receiver<WsInbound>,
    udp_in: cb::Receiver<UdpIn>,
    timeout_in: cb::Receiver<()>,
    next_out: cb::Sender<Instant>,
    answer_out: cb::Sender<String>,
    udp_socket: Arc<std::net::UdpSocket>,
) -> anyhow::Result<()> {
    let mut rtc = build_rtc_config()?;

    let local_addr = SocketAddr::new(local_ip, udp_port);
    let local_candidate = Candidate::host(local_addr, "udp")?;
    rtc.add_local_candidate(local_candidate)
        .ok_or(anyhow::anyhow!("Failed to add local candidate"))?;
    eprintln!("Added local candidate: {local_addr}");

    let offer = str0m::change::SdpOffer::from_sdp_string(&offer_sdp)?;
    let answer = rtc.sdp_api().accept_offer(offer)?;
    let answer_sdp = answer.to_sdp_string();
    eprintln!("SDP answer generated");

    let mut channel_id: Option<str0m::channel::ChannelId> = None;
    let mut messages_to_send: Vec<String> = Vec::new();
    let mut message_index = 0;
    let mut backpressure_state = BackpressureState::new();


    let (mut next_deadline, _, _) = drain_outputs(&mut rtc, udp_socket.as_ref(), &mut channel_id, &mut messages_to_send, &mut message_index, &mut backpressure_state)?;
    let _ = next_out.send(next_deadline);

    let _ = answer_out.send(answer_sdp);

    loop {
        let mut did_work = false;

        while let Ok(ws_msg) = ws_in.try_recv() {
            did_work = true;
            match ws_msg {
                WsInbound::Candidate { candidate, .. } => {
                    try_add_remote_candidate(&mut rtc, &candidate, local_ip)
                }
                WsInbound::EndOfCandidates { .. } => eprintln!("EndOfCandidates received"),
                _ => {}
            }
        }
        // Drain inbound UDP
        while let Ok(UdpIn { buf, src, dst }) = udp_in.try_recv() {
            did_work = true;
            let contents: &[u8] = &buf;
            if let Ok(contents_slice) = contents.try_into() {
                let input = Input::Receive(
                    Instant::now(),
                    Receive {
                        proto: Protocol::Udp,
                        source: src,
                        destination: dst,
                        contents: contents_slice,
                    },
                );
                if let Err(e) = rtc.handle_input(input) {
                    eprintln!("Error handling UDP input: {e:?}");
                    if let Some(source) = e.source() {
                        eprintln!("  Source: {source:?}");
                    }
                }
            }
        }
        while let Ok(()) = timeout_in.try_recv() {
            did_work = true;
            if let Err(e) = rtc.handle_input(Input::Timeout(Instant::now())) {
                eprintln!("Error handling timeout: {e}");
            }
        }

        push_messages(&mut rtc, &channel_id, &messages_to_send, &mut message_index, &mut backpressure_state);

        let (nd, _connected_state, processed_outputs) = drain_outputs(&mut rtc, udp_socket.as_ref(), &mut channel_id, &mut messages_to_send, &mut message_index, &mut backpressure_state)?;
        if processed_outputs {
            did_work = true;
        }
        if nd != next_deadline {
            did_work = true;
            next_deadline = nd;
            let _ = next_out.send(next_deadline);
        }

        // Platform-specific sleep strategy
        platform_sleep(did_work);
    }
}

struct BackpressureState {
    consecutive_errors: usize,
    successful_batches: usize,
    current_batch_size: usize,
    // SCTP-aware flow control
    paused_by_sctp: bool,
    high_water_bytes: usize,
    low_water_bytes: usize,
    last_buffered_bytes: usize,
    platform_config: PlatformConfig,
}

impl BackpressureState {
    fn new() -> Self {
        let platform_config = PlatformConfig::current();
        Self {
            consecutive_errors: 0,
            successful_batches: 0,
            current_batch_size: platform_config.initial_batch_size,
            paused_by_sctp: false,
            high_water_bytes: platform_config.high_water_bytes,
            low_water_bytes: platform_config.low_water_bytes,
            last_buffered_bytes: 0,
            platform_config,
        }
    }

    fn batch_size(&self) -> usize {
        self.current_batch_size
    }

    fn should_pause_for(&self, buffered: usize) -> bool {
        buffered >= self.high_water_bytes
    }

    fn low_threshold(&self) -> usize {
        self.low_water_bytes
    }

    fn adjust_for_backpressure(&mut self, written: usize, had_error: bool, batch_size: usize) {
        if had_error {
            self.consecutive_errors += 1;
            self.successful_batches = 0;
            self.current_batch_size = (self.current_batch_size / 2).max(self.platform_config.min_batch_size);
        } else {
            self.consecutive_errors = 0;
            if written == batch_size && written > 0 {
                self.successful_batches += 1;
                if self.successful_batches >= self.platform_config.success_batches_for_growth
                    && self.current_batch_size < self.platform_config.max_batch_size
                {
                    if self.platform_config.uses_linear_growth() {
                        // conservative growth
                        self.current_batch_size += 1;
                    } else {
                        // exponential growth
                        self.current_batch_size = (self.current_batch_size * 3 / 2).min(self.platform_config.max_batch_size);
                    }
                    self.successful_batches = 0;
                }
            } else if written == 0 && batch_size > 1 {
                self.current_batch_size = (self.current_batch_size * 2 / 3).max(self.platform_config.min_batch_size);
            }
        }
    }

    fn check_backpressure_during_write(&self) -> bool {
        self.platform_config.check_backpressure_during_write
    }

    fn check_backpressure_every_n_writes(&self) -> usize {
        self.platform_config.check_backpressure_every_n_writes
    }
}

fn push_messages(
    rtc: &mut str0m::Rtc,
    channel_id: &Option<str0m::channel::ChannelId>,
    messages_to_send: &[String],
    message_index: &mut usize,
    backpressure_state: &mut BackpressureState,
) {
    if let Some(id) = *channel_id {
        if *message_index >= messages_to_send.len() {
            if *message_index == messages_to_send.len() && !messages_to_send.is_empty() {
                eprintln!("Finished sending all {} messages", messages_to_send.len());
                *message_index += 1;
            }
            return;
        }

        if let Some(mut ch) = rtc.channel(id) {
            // Check if we're paused by SCTP backpressure
            if backpressure_state.paused_by_sctp {
                let buffered_now = ch.buffered_amount();
                if buffered_now < backpressure_state.low_threshold() {
                    backpressure_state.paused_by_sctp = false;
                } else {
                    return;
                }
            }

            // Check backpressure before writing (Windows-specific behavior)
            if backpressure_state.check_backpressure_during_write() {
                let ba = ch.buffered_amount();
                if backpressure_state.should_pause_for(ba) {
                    ch.set_buffered_amount_low_threshold(backpressure_state.low_threshold());
                    backpressure_state.paused_by_sctp = true;
                    return;
                }
            }

            let batch_size = backpressure_state.batch_size();
            let mut written = 0usize;
            let mut had_error = false;
            let check_every_n = backpressure_state.check_backpressure_every_n_writes();

            // Write messages in batches
            while *message_index < messages_to_send.len() && written < batch_size {
                let message = &messages_to_send[*message_index];
                match ch.write(false, message.as_bytes()) {
                    Ok(bytes_written) => {
                        if bytes_written == 0 {
                            had_error = true;
                            break;
                        }
                        *message_index += 1;
                        written += 1;

                        // Check backpressure periodically during batch writes (if configured)
                        if check_every_n > 0 && written % check_every_n == 0 {
                            let ba = ch.buffered_amount();
                            if backpressure_state.should_pause_for(ba) {
                                ch.set_buffered_amount_low_threshold(backpressure_state.low_threshold());
                                backpressure_state.paused_by_sctp = true;
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        if *message_index > 50 {
                            eprintln!("Write error at message {}: {:?}", message_index, e);
                        }
                        had_error = true;
                        break;
                    }
                }
            }

            // Check backpressure after writing a batch
            let ba = ch.buffered_amount();
            backpressure_state.last_buffered_bytes = ba;
            if backpressure_state.should_pause_for(ba) {
                ch.set_buffered_amount_low_threshold(backpressure_state.low_threshold());
                backpressure_state.paused_by_sctp = true;
            }

            backpressure_state.adjust_for_backpressure(written, had_error, batch_size);
        }
    }
}

fn drain_outputs(
    rtc: &mut str0m::Rtc,
    udp_socket: &std::net::UdpSocket,
    channel_id: &mut Option<str0m::channel::ChannelId>,
    messages_to_send: &mut Vec<String>,
    message_index: &mut usize,
    backpressure_state: &mut BackpressureState,
) -> anyhow::Result<(Instant, bool, bool)> {
    let mut connected = false;
    let mut next_timeout = None;
    let mut output_count = 0;
    let max_outputs_per_call = PlatformConfig::current().max_outputs_per_call;

    loop {
        match rtc.poll_output()? {
            Output::Timeout(t) => {
                next_timeout = Some(t);
                push_messages(rtc, channel_id, messages_to_send, message_index, backpressure_state);
                output_count += 1;
                if output_count >= max_outputs_per_call {
                    break;
                }
            },
            Output::Transmit(tx) => {
                if let Err(e) = udp_socket.send_to(&tx.contents, tx.destination) {
                    if is_send_einval_to_non_loopback(&e, tx.destination) {
                        continue;
                    }
                    eprintln!("UDP send_to error to {}: {}", tx.destination, e);
                }
                output_count += 1;
                if output_count >= max_outputs_per_call {
                    break;
                }
            }
            Output::Event(ev) => {
                match &ev {
                Event::IceConnectionStateChange(state) => eprintln!("ICE state: {state:?}"),
                Event::Connected => {
                    eprintln!("WebRTC CONNECTED! ICE and DTLS established.");
                    connected = true;
                }
                Event::ChannelOpen(id, label) => {
                    eprintln!("DataChannel opened: id={id:?}, label={label}");
                    if !connected {
                        eprintln!("Warning: Channel opened but connection not yet established");
                    }
                    *channel_id = Some(*id);
                    if let Some(mut ch) = rtc.channel(*id) {
                        ch.set_buffered_amount_low_threshold(backpressure_state.low_threshold());
                    }
                    // Generate all messages to send (they will be sent one at a time in the Timeout handler)
                    if messages_to_send.is_empty() {
                        eprintln!("Starting to send messages through data channel");
                        // Send message so we can test the server performance
                        // Write messages and continue polling to ensure they're sent
                        // This is critical on Windows where writes are buffered
                        // Queue messages instead of writing them all at once
                        if messages_to_send.is_empty() {
                            eprintln!("Generating messages to send");
                            for i in (10..=510).step_by(10) {
                                for j in (10..=510).step_by(10) {
                                    messages_to_send.push(format!("{j},{i}"));
                                }
                            }
                            eprintln!("Generated {} messages to send", messages_to_send.len());
                        }
                    }
                }
                Event::ChannelData(data) => eprintln!("DataChannel received: {data:?}"),
                Event::ChannelClose(id) => {
                    eprintln!("DataChannel closed: id={id:?}");
                    *channel_id = None;
                }
                Event::ChannelBufferedAmountLow(id2) => {
                    eprintln!("BufferedAmountLow for {:?} — resuming writes", id2);
                    backpressure_state.paused_by_sctp = false;
                    push_messages(rtc, channel_id, messages_to_send, message_index, backpressure_state);
                }
                _ => {}
                }
                output_count += 1;
                if output_count >= max_outputs_per_call {
                    break;
                }
            }
        }
    }

    let timeout = next_timeout.unwrap_or_else(|| Instant::now() + Duration::from_millis(10));
    let processed_any = output_count > 0;
    Ok((timeout, connected, processed_any))
}

fn try_add_remote_candidate(rtc: &mut str0m::Rtc, candidate: &str, local_ip: IpAddr) {
    let line = candidate.trim_start_matches("a=").trim_start_matches("candidate:");
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 6 {
        return;
    }
    if !parts
        .get(2)
        .map(|s| s.eq_ignore_ascii_case("udp"))
        .unwrap_or(false)
    {
        return;
    }

    // ignore mDNS hostnames.
    if parts[4].ends_with(".local") {
        return;
    }

    let ip = match parts.get(4).and_then(|s| s.parse::<IpAddr>().ok()) {
        Some(IpAddr::V4(v4)) => v4,
        _ => return,
    };
    let port = match parts.get(5).and_then(|s| s.parse::<u16>().ok()) {
        Some(p) => p,
        None => return,
    };

    match local_ip {
        IpAddr::V4(v4) if v4.is_loopback() => {
            if !ip.is_loopback() {
                return;
            }
        }
        _ => {
            let o = ip.octets();
            let is_rfc1918 = (o[0] == 10)
                || (o[0] == 172 && (16..=31).contains(&o[1]))
                || (o[0] == 192 && o[1] == 168);
            if !(is_rfc1918 || ip.is_loopback()) {
                return;
            }
        }
    }

    let addr = SocketAddr::new(IpAddr::V4(ip), port);
    if let Ok(c) = Candidate::host(addr, "udp") {
        rtc.add_remote_candidate(c);
    }
}
