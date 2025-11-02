use std::fs::File;
use std::io::BufReader;
use std::net::TcpListener;
use std::path::Path;
use std::sync::Arc;
use std::thread::spawn;

use rustls::{ServerConfig, ServerConnection, StreamOwned};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::pki_types::pem::PemObject;
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

    let listener = TcpListener::bind("127.0.0.1:8000").expect("bind 127.0.0.1:8000");
    eprintln!("tungstenite echo server listening on wss://127.0.0.1:8000");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                eprintln!("incoming TCP connection from {}", stream.peer_addr().unwrap());
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
                            match websocket.read() {
                                Ok(msg) => {
                                    if msg.is_binary() || msg.is_text() {
                                        if let Err(e) = websocket.send(msg) {
                                            eprintln!("send error: {e}");
                                            break;
                                        }
                                    }
                                }
                                Err(e) => {
                                    eprintln!("read error: {e}");
                                    break;
                                }
                            }
                        },
                        Err(e) => {
                            eprintln!("websocket handshake failed: {e}");
                            eprintln!("Hint: Ensure your client connects with wss://localhost:8000 and trusts the local certificate in certs/.");
                        }
                    }
                });
            }
            Err(e) => eprintln!("incoming connection error: {e}"),
        }
    }
}