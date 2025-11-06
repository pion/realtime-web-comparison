// basic str0m implementation for testing purposes.
// uses host candidates, expected to be used in a local network.
// Should work on windows and linux.
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel as cb;
use futures_util::{SinkExt, StreamExt};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use serde::{Deserialize, Serialize};
use tokio::net::{TcpListener, UdpSocket};
use tokio_rustls::{server::TlsStream, TlsAcceptor};
use tokio_tungstenite::{accept_async, tungstenite::Message};

use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output};

#[cfg(all(target_os = "windows", feature = "mimalloc"))]
use mimalloc::MiMalloc;

#[cfg(all(target_os = "windows", feature = "mimalloc"))]
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

#[derive(Debug)]
struct UdpIn {
    buf: Vec<u8>,
    src: SocketAddr,
    dst: SocketAddr,
}

#[cfg(target_os = "windows")]
struct TimerPeriodGuard;
#[cfg(target_os = "windows")]
impl TimerPeriodGuard {
    fn new_1ms() -> Self {
        unsafe { windows::Win32::Media::timeBeginPeriod(1); }
        TimerPeriodGuard
    }
}
#[cfg(target_os = "windows")]
impl Drop for TimerPeriodGuard {
    fn drop(&mut self) {
        unsafe { windows::Win32::Media::timeEndPeriod(1); }
    }
}

// Enumerate non-loopback IPv4s
fn enumerate_ipv4_non_loopback() -> Vec<Ipv4Addr> {
    let mut v = Vec::new();
    if let Ok(ifs) = if_addrs::get_if_addrs() {
        for i in ifs {
            if let IpAddr::V4(ip) = i.ip() {
                if !ip.is_loopback() && !ip.is_link_local() {
                    v.push(ip);
                }
            }
        }
    }
    if v.is_empty() {
        v.push(Ipv4Addr::UNSPECIFIED);
    }
    v
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    #[cfg(target_os = "windows")]
    let _timer_guard = TimerPeriodGuard::new_1ms();

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
        let (tcp, peer_addr) = listener.accept().await?;
        let acceptor = acceptor.clone();

        let bind_ip = if peer_addr.ip().is_loopback() {
            Ipv4Addr::LOCALHOST
        } else {
            enumerate_ipv4_non_loopback()
                .into_iter()
                .find(|ip| *ip != Ipv4Addr::UNSPECIFIED)
                .unwrap_or(Ipv4Addr::LOCALHOST)
        };

        let udp = UdpSocket::bind(SocketAddr::new(IpAddr::V4(bind_ip), 0)).await?;
        let local_udp = udp.local_addr()?;
        eprintln!("UDP bound to: {}", local_udp);

        let std_udp = udp.into_std()?;
        let std_udp_arc = Arc::new(std_udp);

        tokio::spawn(async move {
            if let Err(e) = handle_client(acceptor, tcp, std_udp_arc, local_udp).await {
                eprintln!("client error: {e:?}");
            }
        });
    }
}

async fn handle_client(
    acceptor: TlsAcceptor,
    tcp: tokio::net::TcpStream,
    std_udp_arc: Arc<std::net::UdpSocket>,
    local_udp: SocketAddr,
) -> anyhow::Result<()> {
    let tls: TlsStream<_> = acceptor.accept(tcp).await?;
    let mut ws = accept_async(tls).await?;

    // Expect initial offer
    let first = ws.next().await.ok_or_else(|| anyhow::anyhow!("client closed"))??;
    let offer = match first {
        Message::Text(t) => serde_json::from_str::<WsInbound>(&t)?,
        Message::Binary(b) => serde_json::from_slice::<WsInbound>(&b)?,
        _ => anyhow::bail!("unexpected first WS frame"),
    };
    let offer_sdp = match offer {
        WsInbound::Offer { sdp } => sdp,
        _ => anyhow::bail!("first WS message must be an offer"),
    };

    // Channels
    let (ws_to_rtc_tx, ws_to_rtc_rx) = cb::unbounded::<WsInbound>();
    let (udp_to_rtc_tx, udp_to_rtc_rx) = cb::unbounded::<UdpIn>();
    let (answer_tx, answer_rx) = cb::unbounded::<String>();

    // UDP receiver task
    {
        let std_udp_recv = Arc::clone(&std_udp_arc);
        let local_udp_for_recv = local_udp;
        tokio::spawn(async move {
            let udp_recv =
                tokio::net::UdpSocket::from_std(std_udp_recv.as_ref().try_clone().unwrap()).unwrap();
            let mut buf = vec![0u8; 2000];
            loop {
                match udp_recv.recv_from(&mut buf).await {
                    Ok((n, src)) => {
                        let v = buf[..n].to_vec();
                        let _ = udp_to_rtc_tx.send(UdpIn {
                            buf: v,
                            src,
                            dst: local_udp_for_recv,
                        });
                    }
                    Err(e) => {
                        eprintln!("udp recv error: {e}");
                        break;
                    }
                }
            }
        });
    }

    // Spawn RTC blocking worker; pass the arced socket so it can send directly (no extra send task)
    let offer = offer_sdp.clone();
    let std_udp_for_rtc = Arc::clone(&std_udp_arc);
    tokio::task::spawn_blocking(move || {
        let _ = run_rtc_blocking(
            offer,
            local_udp.ip(),
            local_udp.port(),
            ws_to_rtc_rx,
            udp_to_rtc_rx,
            std_udp_for_rtc, // <— direct sender
            answer_tx,
        );
    });

    // Wait for answer
    let answer = answer_rx.recv().unwrap();

    // Send the SDP answer back to the client
    let answer_obj = serde_json::json!({ "type": "answer", "sdp": answer });
    let _ = ws.send(Message::Text(serde_json::to_string(&answer_obj)?.into())).await;

    let ws_to_rtc_tx_clone = ws_to_rtc_tx.clone();
    let mut ws_read = ws;
    tokio::spawn(async move {
        while let Some(msg) = ws_read.next().await {
            match msg {
                Ok(Message::Text(t)) => {
                    if let Ok(v) = serde_json::from_str::<WsInbound>(&t) {
                        let _ = ws_to_rtc_tx_clone.send(v);
                    }
                }
                Ok(Message::Binary(b)) => {
                    if let Ok(v) = serde_json::from_slice::<WsInbound>(&b) {
                        let _ = ws_to_rtc_tx_clone.send(v);
                    }
                }
                Ok(Message::Close(_)) | Err(_) => break,
                _ => {}
            }
        }
    });

    Ok(())
}

fn run_rtc_blocking(
    offer_sdp: String,
    local_ip: IpAddr,
    udp_port: u16,
    ws_in: cb::Receiver<WsInbound>,
    udp_in: cb::Receiver<UdpIn>,
    udp_sock: Arc<std::net::UdpSocket>, // <— direct sender
    answer_tx: cb::Sender<String>,
) -> anyhow::Result<()> {
    use std::collections::HashSet;
    let cert_options = str0m::config::DtlsCertOptions {
        pkey_type: str0m::config::DtlsPKeyType::EcDsaP256,
        ..Default::default()
    };
    let crypto_provider = if cfg!(windows) {
        str0m::config::CryptoProvider::WinCrypto
    } else {
        str0m::config::CryptoProvider::OpenSsl
    };
    let dtls_cert = str0m::config::DtlsCert::new(crypto_provider, cert_options);
    let str0m_config = str0m::RtcConfig::new()
        .set_dtls_cert_config(str0m::DtlsCertConfig::PregeneratedCert(dtls_cert));

    let mut rtc = str0m_config.build();

    let local_addr = SocketAddr::new(local_ip, udp_port);
    let localhost_cand = Candidate::host(local_addr, "udp")?;
    rtc
        .add_local_candidate(localhost_cand)
        .ok_or_else(|| anyhow::anyhow!("failed to add local candidate"))?;
    eprintln!("Added local candidate: {local_addr}");

    let offer = str0m::change::SdpOffer::from_sdp_string(&offer_sdp)?;

    let answer = rtc.sdp_api().accept_offer(offer)?.to_sdp_string();
    eprintln!("SDP answer generated");

    loop {
        match rtc.poll_output()? {
            Output::Timeout(when) => {
                let now = Instant::now();
                if now >= when {
                    rtc.handle_input(Input::Timeout(now))?;
                } else {
                    break;
                }
            }
            Output::Transmit(tx) => {
                // Send directly on the arced socket
                let _ = udp_sock.send_to(&tx.contents, tx.destination);
            }
            Output::Event(ev) => {
                if let Event::IceConnectionStateChange(st) = &ev {
                    eprintln!("ICE state after accept: {st:?}");
                }
            }
        }
    }

    // Provide answer to WS task
    let _ = answer_tx.send(answer);

    let mut seen_ws_candidates = HashSet::<SocketAddr>::new();

    loop {
        // Drain str0m outputs until timeout
        let next_when = loop {
            match rtc.poll_output()? {
                Output::Timeout(when) => break when,
                Output::Transmit(tx) => {
                    // During early ICE, sends to “other” pairs can legitimately fail on Windows.
                    // We best-effort send and ignore OS error 10051 noise.
                    let _ = udp_sock.send_to(&tx.contents, tx.destination);
                }
                Output::Event(ev) => {
                    match &ev {
                        Event::Connected => {
                            eprintln!("WebRTC CONNECTED! ICE and DTLS established.");
                        }
                        Event::IceConnectionStateChange(state) => {
                            eprintln!("ICE state changed: {state:?}");
                            if matches!(state, IceConnectionState::Disconnected) {
                                eprintln!("ICE DISCONNECTED - stopping.");
                                return Ok(());
                            }
                        }
                        Event::ChannelOpen(id, label) => {
                            eprintln!("DataChannel opened: id={id:?}, label={label}");
                            if let Some(mut ch) = rtc.channel(*id) {
                                // send a couple of probes
                                for i in 0..5 {
                                    let _ = ch.write(true, format!("hi {i}").as_bytes());
                                }
                            }
                        }
                        Event::ChannelData(data) => {
                            eprintln!("DataChannel received {} bytes", data.data.len());
                        }
                        _ => {}
                    }
                }
            }
        };

        // drain WS
        while let Ok(msg) = ws_in.try_recv() {
            match msg {
                WsInbound::Candidate { candidate, .. } => {
                    let candidate_line = candidate
                        .strip_prefix("a=")
                        .or_else(|| candidate.strip_prefix("candidate:"))
                        .unwrap_or(&candidate);
                    let parts: Vec<&str> = candidate_line.split_whitespace().collect();
                    if parts.len() >= 8 && parts[2].eq_ignore_ascii_case("udp") {
                        if let (Ok(ip), Ok(port)) =
                            (parts[4].parse::<IpAddr>(), parts[5].parse::<u16>())
                        {
                            if !ip.is_unspecified() {
                                let addr = SocketAddr::new(ip, port);
                                if seen_ws_candidates.insert(addr) {
                                    if let Ok(c) = Candidate::host(addr, "udp") {
                                        let _ = rtc.add_remote_candidate(c);
                                    }
                                }
                            }
                        }
                    }
                }
                WsInbound::EndOfCandidates {} => { }
                WsInbound::Offer { .. } => { }
            }
        }

        // drain UDP
        let mut fed_any = false;
        while let Ok(UdpIn { buf, src, dst }) = udp_in.try_recv() {
            let input = Input::Receive(
                Instant::now(),
                Receive {
                    proto: Protocol::Udp,
                    source: src,
                    destination: dst,
                    contents: buf.as_slice().try_into().unwrap(),
                },
            );
            rtc.handle_input(input)?;
            fed_any = true;
        }
        if fed_any {
            // after receive bursts, we may get immediate outputs?
            continue;
        }

        // wait/sleep until next timeout
        let now = Instant::now();
        if now >= next_when {
            rtc.handle_input(Input::Timeout(now))?;
        } else {
            let sleep = (next_when - now).min(Duration::from_millis(1));
            std::thread::sleep(sleep);
            rtc.handle_input(Input::Timeout(Instant::now()))?;
        }
    }
}
