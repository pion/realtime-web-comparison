// str0m test server with a single tokio::select! loop (WS, UDP, timer)
// should keeps the WebSocket/TLS flow unchanged as other clients. tested on Windows & Linux by.
// doesn't work on firefox on windows (dtls handshake fails).
use std::error::Error;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel as cb;
use futures_util::{SinkExt, StreamExt};
use if_addrs::get_if_addrs;
use mimalloc::MiMalloc;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use serde::{Deserialize, Serialize};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output};
use tokio::net::{TcpListener, UdpSocket};
use tokio::time::{sleep_until, Instant as TokioInstant};
use tokio_rustls::{server::TlsStream, TlsAcceptor};
use tokio_tungstenite::{accept_async, tungstenite::Message};

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;


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

    let listener = TcpListener::bind("127.0.0.1:8002").await?;
    eprintln!("Listening on wss://127.0.0.1:8002");

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

// This function handles one WebRTC connection from start to finish
async fn handle_client(
    acceptor: TlsAcceptor,
    tcp: tokio::net::TcpStream,
) -> anyhow::Result<()> {
    let tls: TlsStream<_> = acceptor.accept(tcp).await?;
    let mut ws = accept_async(tls).await?;

    let first_msg = ws.next().await.ok_or_else(|| anyhow::anyhow!("client closed"))??;
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

    let mut buf = vec![0u8; 2048];
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
    let mut rtc = {
        #[cfg(windows)]
        {
            // trying to fix it for firefox on windows?
            let cert_opts = str0m::config::DtlsCertOptions {
                pkey_type: str0m::config::DtlsPKeyType::Rsa2048,
                ..Default::default()
            };
            let crypto_provider = str0m::config::CryptoProvider::WinCrypto;
            let dtls_cert = str0m::config::DtlsCert::new(crypto_provider, cert_opts);
            let str0m_config = str0m::RtcConfig::new()
                .set_dtls_cert_config(str0m::DtlsCertConfig::PregeneratedCert(dtls_cert));
            str0m_config.build()
        }
        #[cfg(not(windows))]
        {
            str0m::RtcConfig::new().build()
        }
    };

    let local_addr = SocketAddr::new(local_ip, udp_port);
    let local_candidate = Candidate::host(local_addr, "udp")?;
    rtc.add_local_candidate(local_candidate)
        .ok_or(anyhow::anyhow!("Failed to add local candidate"))?;
    eprintln!("Added local candidate: {local_addr}");

    let offer = str0m::change::SdpOffer::from_sdp_string(&offer_sdp)?;
    let answer = rtc.sdp_api().accept_offer(offer)?;
    let answer_sdp = answer.to_sdp_string();
    eprintln!("SDP answer generated");

    // Track data channel state for sending messages
    let mut channel_id: Option<str0m::channel::ChannelId> = None;
    let mut messages_to_send: Vec<String> = Vec::new();
    let mut message_index = 0;

    let (mut next_deadline, _) = drain_outputs(&mut rtc, udp_socket.as_ref(), &mut channel_id, &mut messages_to_send, &mut message_index)?;
    let _ = next_out.send(next_deadline);

    let _ = answer_out.send(answer_sdp);

    loop {
        while let Ok(ws_msg) = ws_in.try_recv() {
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
            if let Err(e) = rtc.handle_input(Input::Timeout(Instant::now())) {
                eprintln!("Error handling timeout: {e}");
            }
        }

        let (nd, connected_state) = drain_outputs(&mut rtc, udp_socket.as_ref(), &mut channel_id, &mut messages_to_send, &mut message_index)?;
        if nd != next_deadline {
            next_deadline = nd;
            let _ = next_out.send(next_deadline);
        }

        std::thread::sleep(Duration::from_millis(1));
    }
}

fn drain_outputs(
    rtc: &mut str0m::Rtc,
    udp_socket: &std::net::UdpSocket,
    channel_id: &mut Option<str0m::channel::ChannelId>,
    messages_to_send: &mut Vec<String>,
    message_index: &mut usize,
) -> anyhow::Result<(Instant, bool)> {
    let mut connected = false;
    loop {
        match rtc.poll_output()? {
            Output::Timeout(t) => {
                // Try to send next message if channel is open and we have messages
                if let Some(id) = *channel_id {
                    if *message_index < messages_to_send.len() {
                        // Get channel, write message, drop channel before continuing
                        if let Some(mut ch) = rtc.channel(id) {
                            let message = &messages_to_send[*message_index];
                            match ch.write(false, message.as_bytes()) {
                                Ok(_bytes) => {
                                    *message_index += 1;
                                    if *message_index % 100 == 0 {
                                        eprintln!("Sent {} messages", message_index);
                                    }
                                }
                                Err(e) => {
                                    eprintln!("DataChannel write error (message {}): {e}", message_index);
                                }
                            }
                        } else {
                            // Channel not available - skip this message and try again next time
                            eprintln!("Channel not available, skipping message {}", message_index);
                        }
                    } else if *message_index == messages_to_send.len() && !messages_to_send.is_empty() {
                        eprintln!("Finished sending all {} messages", messages_to_send.len());
                        *message_index += 1; // Prevent repeated logging
                    }
                }
                break Ok((t, connected));
            },
            Output::Transmit(tx) => {
                if let Err(e) = udp_socket.send_to(&tx.contents, tx.destination) {
                    if is_send_einval_to_non_loopback(&e, tx.destination) {
                        continue;
                    }
                    eprintln!("UDP send_to error to {}: {}", tx.destination, e);
                }
            }
            Output::Event(ev) => match &ev {
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
                _ => {}
            },
        }
    }
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
