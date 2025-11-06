use std::io::ErrorKind;
use std::net::{TcpListener, UdpSocket};
use std::path::Path;
use std::sync::Arc;
use std::thread::spawn;
use std::time::{Duration, Instant};

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use str0m::change::SdpOffer;
use str0m::config::CryptoProvider;
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};
use tungstenite::{accept, buffer};

/// A WebSocket echo server over TLS (wss://)
fn main() {
    // Use fixed relative paths from this crate to the repo certs
    let cert_path = Path::new("../../certs/localhost.pem");
    let key_path = Path::new("../../certs/localhost-key.pem");

    eprintln!("Using certificate: {}", cert_path.display());
    eprintln!("Using private key: {}", key_path.display());

    let cert = CertificateDer::from_pem_file(cert_path).expect("load certs");
    let key = PrivateKeyDer::from_pem_file(key_path).expect("load private key");

    // Build rustls server config (no client auth)
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .expect("invalid cert/key");
    let config = Arc::new(config);

    let listener = TcpListener::bind("127.0.0.1:8002").expect("bind 127.0.0.1:8002");
    eprintln!("tungstenite server listening on wss://127.0.0.1:8002");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                eprintln!(
                    "incoming TCP connection from {}",
                    stream.peer_addr().unwrap()
                );
                let cfg = Arc::clone(&config);
                spawn(move || {
                    // Wrap TCP in a rustls TLS stream.
                    let conn = match ServerConnection::new(cfg) {
                        Ok(c) => c,
                        Err(e) => {
                            eprintln!("TLS ServerConnection error: {e}");
                            return;
                        }
                    };
                    let tls_stream = StreamOwned::new(conn, stream);

                    match accept(tls_stream) {
                        Ok(mut websocket) => loop {
                            if let Ok(msg) = websocket.read() {
                                eprintln!("Received message: {}", msg);

                                // Parse remote SDP offer.
                                let offer: SdpOffer = serde_json::from_slice(&msg.into_data())
                                    .expect("serialized offer");

                                // Prepare UDP socket to carry ICE/DTLS/SCTP traffic.
                                let udp = UdpSocket::bind("0.0.0.0:0").expect("bind UDP");
                                let local_addr = udp.local_addr().expect("local addr");
                                eprintln!("UDP bound on {}", local_addr);

                                // Build Rtc instance.
                                let cert_options = str0m::config::DtlsCertOptions {
                                    pkey_type: str0m::config::DtlsPKeyType::EcDsaP256,
                                    ..Default::default()
                                };
                                let crypto_provider = if cfg!(windows) {
                                    str0m::config::CryptoProvider::WinCrypto
                                } else {
                                    str0m::config::CryptoProvider::OpenSsl
                                };
                                let dtls_cert =
                                    str0m::config::DtlsCert::new(crypto_provider, cert_options);

                                let str0m_config = str0m::RtcConfig::new().set_dtls_cert_config(
                                    str0m::DtlsCertConfig::PregeneratedCert(dtls_cert),
                                );

                                let mut rtc = str0m_config.build();

                                // Accept offer and create answer.
                                let answer = rtc
                                    .sdp_api()
                                    .accept_offer(offer)
                                    .expect("offer to be accepted");

                                // Send SDP answer back to the client.
                                websocket
                                    .write(answer.to_sdp_string().into())
                                    .expect("send answer");
                                eprintln!("Sent SDP answer");

                                let socket = udp;
                                let buffer_size = 2000;
                                let mut buf = vec![0u8; buffer_size];
                                loop {
                                    // Poll outputs until timeout is returned.
                                    let timeout_instant = loop {
                                        match rtc.poll_output().expect("poll output") {
                                            Output::Timeout(when) => break when,
                                            Output::Transmit(tx) => {
                                                let _ =
                                                    socket.send_to(&tx.contents, tx.destination);
                                            }
                                            Output::Event(ev) => {
                                                eprintln!("RTC event: {:?}", ev);
                                                if let Event::IceConnectionStateChange(state) = ev {
                                                    if state == IceConnectionState::Disconnected {
                                                        eprintln!("ICE disconnected");
                                                        return;
                                                    }
                                                }
                                            }
                                        }
                                    };

                                    // Compute duration until timeout.
                                    let now = Instant::now();
                                    let duration = if timeout_instant > now {
                                        timeout_instant - now
                                    } else {
                                        Duration::from_millis(0)
                                    };

                                    if duration.is_zero() {
                                        // Drive timers immediately.
                                        rtc.handle_input(Input::Timeout(Instant::now()))
                                            .expect("timeout input");
                                        continue;
                                    }

                                    // run RTC input
                                    let _ = socket.set_read_timeout(Some(duration));
                                    buf.resize(buffer_size, 0);
                                    match socket.recv_from(&mut buf) {
                                        Ok((n, src)) => {
                                            buf.truncate(n);
                                            let input = Input::Receive(
                                                Instant::now(),
                                                Receive {
                                                    proto: Protocol::Udp,
                                                    source: src,
                                                    destination: socket
                                                        .local_addr()
                                                        .expect("local addr"),
                                                    contents: buf
                                                        .as_slice()
                                                        .try_into()
                                                        .expect("slice"),
                                                },
                                            );
                                            rtc.handle_input(input).expect("handle input");
                                        }
                                        Err(e)
                                            if e.kind() == ErrorKind::WouldBlock
                                                || e.kind() == ErrorKind::TimedOut =>
                                        {
                                            rtc.handle_input(Input::Timeout(Instant::now()))
                                                .expect("timeout input");
                                        }
                                        Err(e) => {
                                            eprintln!("UDP error: {e}");
                                            return;
                                        }
                                    }
                                }
                            } else {
                                eprintln!("Client disconnected");
                                break;
                            }
                        },
                        Err(e) => {
                            eprintln!("websocket handshake failed: {e}");
                            eprintln!(
                                "Hint: Ensure your client connects with wss://localhost:8002 and trusts the local certificate in certs/."
                            );
                        }
                    }
                });
            }
            Err(e) => eprintln!("incoming connection error: {e}"),
        }
    }
}
