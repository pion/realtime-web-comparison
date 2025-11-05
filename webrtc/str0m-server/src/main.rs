use std::net::TcpListener;
use std::path::Path;
use std::sync::Arc;
use std::thread::spawn;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use str0m::Rtc;
use str0m::change::SdpOffer;
use tungstenite::accept;

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
                                // create RTC client
                                let offer: SdpOffer =
                                    serde_json::from_slice(&msg.into_data()).expect("serialized offer");
                                let mut rtc = Rtc::builder()
                                    // Uncomment this to see statistics
                                    // .set_stats_interval(Some(Duration::from_secs(1)))
                                    // .set_ice_lite(true)
                                    .build();

                                // Add the shared UDP socket as a host candidate
                                /*
                                let candidate =
                                    Candidate::host(addr, "udp").expect("a host candidate");
                                rtc.add_local_candidate(candidate).unwrap();
                                */

                                // Create an SDP Answer.
                                let answer = rtc
                                    .sdp_api()
                                    .accept_offer(offer)
                                    .expect("offer to be accepted");

                                websocket.write(answer.to_sdp_string().into()).expect("send answer");
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
