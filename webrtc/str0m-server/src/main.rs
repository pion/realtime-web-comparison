use std::net::TcpListener;
use std::path::Path;
use std::sync::Arc;
use std::thread::spawn;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use str0m::change::SdpOffer;
use str0m::{Candidate, Rtc};
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
                                let offer: SdpOffer = serde_json::from_slice(&msg.into_data())
                                    .expect("serialized offer");
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

                                // Create an SDP Answer.
                                let answer = rtc
                                    .sdp_api()
                                    .accept_offer(offer)
                                    .expect("offer to be accepted");

                                websocket
                                    .write(answer.to_sdp_string().into())
                                    .expect("send answer");

                                eprintln!("Sent SDP answer");
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
