// Cargo.toml (dependencies omitted for brevity)...

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

// Enumerate non-loopback IPv4 addresse
#[allow(dead_code)]
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

#[cfg(target_os = "windows")]
struct TimerPeriodGuard;
#[cfg(target_os = "windows")]
impl TimerPeriodGuard {
    fn new_1ms() -> Self {
        unsafe { windows_sys::Win32::Media::Multimedia::timeBeginPeriod(1); }
        TimerPeriodGuard
    }
}
#[cfg(target_os = "windows")]
impl Drop for TimerPeriodGuard {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Media::Multimedia::timeEndPeriod(1); }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    #[cfg(target_os = "windows")]
    let _timer_guard = TimerPeriodGuard::new_1ms();

    // TLS setup omitted for brevity...
    let cert_path = Path::new("../../certs/localhost.pem");
    let key_path = Path::new("../../certs/localhost-key.pem");
    let cert_pem = std::fs::read(cert_path)?;
    let key_pem = std::fs::read(key_path)?;
    let cert = rustls_pemfile::certs(&mut &cert_pem[..]).next()
        .ok_or_else(|| anyhow::anyhow!("no certificate found"))?
        .map(CertificateDer::from)?;
    let key = rustls_pemfile::pkcs8_private_keys(&mut &key_pem[..]).next()
        .ok_or_else(|| anyhow::anyhow!("no private key found"))?
        .map(PrivateKeyDer::from)?;
    let rustls_cfg = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)?;
    let acceptor = TlsAcceptor::from(Arc::new(rustls_cfg));

    // Choose a bind address: first non-loopback IPv4, or localhost if none
    let bind_ip = match enumerate_ipv4_non_loopback().into_iter().find(|&ip| ip != Ipv4Addr::UNSPECIFIED) {
        Some(ip) => ip,
        None => Ipv4Addr::LOCALHOST,
    };
    // Bind UDP on chosen address and an ephemeral port
    let udp = UdpSocket::bind(SocketAddr::new(IpAddr::V4(bind_ip), 0)).await?;
    let local_udp = udp.local_addr()?;
    eprintln!("UDP bound to: {}", local_udp);

    // Shared UDP socket for use in str0m loop
    let std_udp = udp.into_std()?;
    let std_udp_arc = Arc::new(std_udp);

    // TCP listener for WSS
    let listener = TcpListener::bind("127.0.0.1:8002").await?;
    eprintln!("Listening on wss://127.0.0.1:8002");

    loop {
        let (tcp, _) = listener.accept().await?;
        let acceptor = acceptor.clone();
        let std_udp_arc_clone = Arc::clone(&std_udp_arc);

        tokio::spawn(async move {
            if let Err(e) = handle_client(acceptor, tcp, std_udp_arc_clone, local_udp).await {
                eprintln!("Client error: {:?}", e);
            }
        });
    }
}

// Handle one WebSocket client
async fn handle_client(
    acceptor: TlsAcceptor,
    tcp: tokio::net::TcpStream,
    std_udp_arc: Arc<std::net::UdpSocket>,
    local_udp: SocketAddr,
) -> anyhow::Result<()> {
    // Upgrade to TLS and WebSocket
    let tls: TlsStream<_> = acceptor.accept(tcp).await?;
    let mut ws = accept_async(tls).await?;

    // Expect initial offer
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

    // Channels for inter-thread communication
    let (ws_to_rtc_tx, ws_to_rtc_rx) = cb::unbounded::<WsInbound>();
    let (udp_to_rtc_tx, udp_to_rtc_rx) = cb::unbounded::<UdpIn>();
    let (rtc_to_udp_tx, rtc_to_udp_rx) = cb::unbounded::<UdpOut>();
    let (answer_tx, answer_rx) = cb::unbounded::<String>();

    {
        let std_udp_send = Arc::clone(&std_udp_arc);
        tokio::task::spawn_blocking(move || {
            for out in rtc_to_udp_rx.iter() {
                if let Err(e) = std_udp_send.send_to(&out.buf, out.dst) {
                    eprintln!("UDP send error: {}", e);
                }
            }
        });
    }

    // UDP receiver task
    {
        let std_udp_recv = Arc::clone(&std_udp_arc);
        let local_addr_for_recv = local_udp;
        tokio::spawn(async move {
            let udp_recv = tokio::net::UdpSocket::from_std(std_udp_recv.as_ref().try_clone().unwrap()).unwrap();
            let mut buf = vec![0u8; 2000];
            loop {
                match udp_recv.recv_from(&mut buf).await {
                    Ok((n, src)) => {
                        let data = buf[..n].to_vec();
                        let _ = udp_to_rtc_tx.send(UdpIn { buf: data, src, dst: local_addr_for_recv });
                    }
                    Err(e) => {
                        eprintln!("UDP receive error: {e}");
                        break;
                    }
                }
            }
        });
    }

    // spawn the blocking WebRTC (str0m) worker
    let offer_clone = offer_sdp.clone();
    let local_ip_for_rtc = local_udp.ip();
    let local_port_for_rtc = local_udp.port();
    tokio::task::spawn_blocking(move || {
        let _ = run_rtc_blocking(
            offer_clone,
            local_ip_for_rtc,
            local_port_for_rtc,
            ws_to_rtc_rx,
            udp_to_rtc_rx,
            rtc_to_udp_tx,
            answer_tx,
        );
    });

    // Wait for SDP answer from str0m
    let answer = answer_rx.recv().unwrap();

    // Send the SDP answer back to client
    let answer_obj = serde_json::json!({ "type": "answer", "sdp": answer });
    let _ = ws.send(Message::Text(serde_json::to_string(&answer_obj)?.into())).await;
    // Send end-of-candidates as well
    let end_msg = WsOutbound::EndOfCandidates {};
    let _ = ws.send(Message::Text(serde_json::to_string(&end_msg)?.into())).await;

    // read remaining ICE candidates from WebSocket (trickle ICE)
    let ws_to_rtc_tx_clone = ws_to_rtc_tx.clone();
    let mut ws_reader = ws;
    tokio::spawn(async move {
        while let Some(msg) = ws_reader.next().await {
            match msg {
                Ok(Message::Text(t)) => {
                    if let Ok(inb) = serde_json::from_str::<WsInbound>(&t) {
                        let _ = ws_to_rtc_tx_clone.send(inb);
                    }
                }
                Ok(Message::Binary(b)) => {
                    if let Ok(inb) = serde_json::from_slice::<WsInbound>(&b) {
                        let _ = ws_to_rtc_tx_clone.send(inb);
                    }
                }
                Ok(Message::Close(_)) | Err(_) => break,
                _ => {}
            }
        }
    });

    Ok(())
}

// Data structures for UDP channel (no change)
#[derive(Debug)]
struct UdpIn { buf: Vec<u8>, src: SocketAddr, dst: SocketAddr }
#[derive(Debug)]
struct UdpOut { buf: Vec<u8>, dst: SocketAddr }

// The str0m run-loop (blocking). Receives the local IP and port.
fn run_rtc_blocking(
    offer_sdp: String,
    local_ip: IpAddr,
    udp_port: u16,
    ws_in: cb::Receiver<WsInbound>,
    udp_in: cb::Receiver<UdpIn>,
    udp_out: cb::Sender<UdpOut>,
    answer_tx: cb::Sender<String>,
) -> anyhow::Result<()> {
    // Build str0m RTC with DTLS cert (unchanged)...
    let cert_opts = str0m::config::DtlsCertOptions {
        pkey_type: str0m::config::DtlsPKeyType::EcDsaP256,
        ..Default::default()
    };
    let crypto_provider = if cfg!(windows) {
        str0m::config::CryptoProvider::WinCrypto
    } else {
        str0m::config::CryptoProvider::OpenSsl
    };
    let dtls_cert = str0m::config::DtlsCert::new(crypto_provider, cert_opts);
    let str0m_config = str0m::RtcConfig::new().set_dtls_cert_config(str0m::DtlsCertConfig::PregeneratedCert(dtls_cert));
    let mut rtc = str0m_config.build();

    // Add local host candidate using the actual local IP
    let local_addr = SocketAddr::new(local_ip, udp_port);
    let local_candidate = Candidate::host(local_addr, "udp")?;
    rtc.add_local_candidate(local_candidate).ok_or(anyhow::anyhow!("Failed to add local candidate"))?;
    eprintln!("Added local candidate: {}", local_addr);

    // Parse offer SDP
    let offer = str0m::change::SdpOffer::from_sdp_string(&offer_sdp)?;

    // Extract and add remote candidates from SDP
    let mut remote_count = 0;
    for line in offer_sdp.lines() {
        if let Some(cand_line) = line.strip_prefix("a=candidate:") {
            let parts: Vec<&str> = cand_line.split_whitespace().collect();
            if parts.len() >= 8 && parts[2] == "UDP" {
                let ip_str = parts[4];
                let port_str = parts[5];
                if !ip_str.ends_with(".local") {
                    if let (Ok(ip), Ok(port)) = (ip_str.parse::<IpAddr>(), port_str.parse::<u16>()) {
                        if !ip.is_unspecified() {
                            let addr = SocketAddr::new(ip, port);
                            if let Ok(c) = Candidate::host(addr, "udp") {
                                rtc.add_remote_candidate(c);
                                remote_count += 1;
                                eprintln!("Added remote candidate from SDP: {}", addr);
                            }
                        }
                    }
                }
            }
        }
    }
    eprintln!("Total remote candidates added from offer: {}", remote_count);

    // Accept the offer to create answer SDP
    let answer = rtc.sdp_api().accept_offer(offer)?;
    let answer_sdp = answer.to_sdp_string();
    eprintln!("SDP answer generated");

    // Initial ICE processing (to kick-start connectivity checks)
    let mut done_initial = false;
    while !done_initial {
        match rtc.poll_output()? {
            Output::Timeout(t) => {
                if Instant::now() < t {
                    done_initial = true;
                } else {
                    rtc.handle_input(Input::Timeout(Instant::now()))?;
                }
            }
            Output::Transmit(tx) => {
                udp_out.send(UdpOut { buf: tx.contents.to_vec(), dst: tx.destination })?;
            }
            Output::Event(ev) => {
                match ev {
                    Event::IceConnectionStateChange(state) => {
                        eprintln!("ICE state after accept: {:?}", state);
                    }
                    _ => {}
                }
            }
        }
    }

    // Send the answer SDP back to the main thread for WebSocket delivery
    answer_tx.send(answer_sdp)?;

    // Main loop: handle ICE, DataChannel, and trickled candidates
    let mut seen_ws_candidates = std::collections::HashSet::new();
    loop {
        // First, drain any str0m output until a timeout
        let next_time = loop {
            match rtc.poll_output()? {
                Output::Timeout(t) => break t,
                Output::Transmit(tx) => {
                    udp_out.send(UdpOut { buf: tx.contents.to_vec(), dst: tx.destination })?;
                }
                Output::Event(ev) => {
                    match &ev {
                        Event::Connected => {
                            eprintln!("WebRTC CONNECTED! ICE and DTLS established.");
                        }
                        Event::IceConnectionStateChange(state) => {
                            eprintln!("ICE state changed: {:?}", state);
                            if matches!(state, IceConnectionState::Disconnected) {
                                eprintln!("ICE disconnected, stopping.");
                                return Ok(());
                            }
                        }
                        Event::ChannelOpen(id, label) => {
                            eprintln!("DataChannel opened: id={:?}, label={}", id, label);
                            // (Example: send messages on the new channel)
                            if let Some(mut channel) = rtc.channel(*id) {
                                for i in (10..=100).step_by(10) {
                                    let msg = format!("Hello {}", i);
                                    channel.write(false, msg.as_bytes()).unwrap();
                                }
                            }
                        }
                        Event::ChannelData(data) => {
                            eprintln!("DataChannel received: {:?}", data);
                        }
                        Event::ChannelClose(id) => {
                            eprintln!("DataChannel closed: id={:?}", id);
                        }
                        _ => {}
                    }
                }
            }
        };

        // Handle any incoming WS (trickle ICE) messages
        while let Ok(ws_msg) = ws_in.try_recv() {
            match ws_msg {
                WsInbound::Candidate { candidate, .. } => {
                    // Parse and add remote candidate from WebSocket
                    let candidate_line = candidate
                        .strip_prefix("a=")
                        .or_else(|| candidate.strip_prefix("candidate:"))
                        .unwrap_or(&candidate);
                    let parts: Vec<&str> = candidate_line.split_whitespace().collect();
                    if parts.len() >= 8 && parts[2] == "UDP" {
                        if let (Ok(ip), Ok(port)) = (parts[4].parse::<IpAddr>(), parts[5].parse::<u16>()) {
                            if !ip.is_unspecified() {
                                let addr = SocketAddr::new(ip, port);
                                if !seen_ws_candidates.contains(&addr) {
                                    if let Ok(c) = Candidate::host(addr, "udp") {
                                        rtc.add_remote_candidate(c);
                                        seen_ws_candidates.insert(addr);
                                        eprintln!("Added remote candidate from WS: {}", addr);
                                    }
                                }
                            }
                        }
                    }
                }
                WsInbound::EndOfCandidates { .. } => {
                    // No action needed; str0m handles end-of-candidates internally
                }
                _ => {}
            }
        }

        // Handle any incoming UDP packets
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
        }

        // Wait until next timeout
        let now = Instant::now();
        if now < next_time {
            std::thread::sleep((next_time - now).min(Duration::from_millis(1)));
            rtc.handle_input(Input::Timeout(Instant::now()))?;
        } else {
            rtc.handle_input(Input::Timeout(Instant::now()))?;
        }
    }
}
