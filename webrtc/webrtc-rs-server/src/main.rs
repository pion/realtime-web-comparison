use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, LazyLock, OnceLock};
use std::thread::spawn;

use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use tungstenite::accept;

use tokio::time::Duration;
use webrtc::api::APIBuilder;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::{RTCPeerConnection, math_rand_alpha};

static PEER_CONNECTION: OnceLock<Arc<RTCPeerConnection>> = OnceLock::new();

/// A WebSocket echo server over TLS (wss://)
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    CryptoProvider::install_default(rustls::crypto::ring::default_provider()).unwrap();
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
                tokio::spawn(async move {
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
                        Ok(mut websocket) => {
                            // Create a MediaEngine object to configure the supported codec
                            let mut m = MediaEngine::default();

                            // Register default codecs
                            m.register_default_codecs().unwrap();

                            // Create a InterceptorRegistry. This is the user configurable RTP/RTCP Pipeline.
                            // This provides NACKs, RTCP Reports and other features. If you use `webrtc.NewPeerConnection`
                            // this is enabled by default. If you are manually managing You MUST create a InterceptorRegistry
                            // for each PeerConnection.
                            let mut registry = Registry::new();

                            // Use the default set of Interceptors
                            registry = register_default_interceptors(registry, &mut m).unwrap();

                            // Create the API object with the MediaEngine
                            let api = APIBuilder::new()
                                .with_media_engine(m)
                                .with_interceptor_registry(registry)
                                .build();

                            // Prepare the configuration
                            let config = RTCConfiguration {
                                ice_servers: vec![RTCIceServer {
                                    urls: vec!["stun:stun.l.google.com:19302".to_owned()],
                                    ..Default::default()
                                }],
                                ..Default::default()
                            };

                            // Create a new RTCPeerConnection
                            let peer_connection =
                                Arc::new(api.new_peer_connection(config).await.unwrap());

                            // Set the handler for Peer connection state
                            // This will notify you when the peer has connected/disconnected
                            peer_connection.on_peer_connection_state_change(Box::new(
                                move |s: RTCPeerConnectionState| {
                                    println!("Peer Connection State has changed: {s}");

                                    if s == RTCPeerConnectionState::Failed {
                                        // Wait until PeerConnection has had no network activity for 30 seconds or another failure. It may be reconnected using an ICE Restart.
                                        // Use webrtc.PeerConnectionStateDisconnected if you are interested in detecting faster timeout.
                                        // Note that the PeerConnection may come back from PeerConnectionStateDisconnected.
                                        println!("Peer Connection has gone to failed exiting");
                                    }

                                    Box::pin(async {})
                                },
                            ));

                            peer_connection.on_ice_candidate(Box::new(
                                move |candidate: Option<RTCIceCandidate>| {
                                    Box::pin(async move {
                                        if let Some(c) = candidate {
                                            println!(
                                                "New ICE candidate: {}",
                                                c.to_json().unwrap().candidate
                                            );
                                        } else {
                                            println!("ICE gathering complete");
                                        }
                                    })
                                },
                            ));

                            peer_connection.on_data_channel(Box::new(
                                move |data_channel: Arc<RTCDataChannel>| {
                                    println!(
                                        "New DataChannel {}-{}",
                                        data_channel.label(),
                                        data_channel.id()
                                    );
                                    Box::pin(async move {})
                                },
                            ));

                            let msg = websocket.read_message().expect("read message");
                            let sdp_bytes = msg.into_text().expect("msg to text");
                            let offer: RTCSessionDescription =
                                serde_json::from_str(&sdp_bytes).expect("unmarshal SDP");
                            println!("Received offer: {offer:?}");

                            // Apply the offer as the remote description
                            peer_connection.set_remote_description(offer).await.unwrap();

                            // Create an answer to send to the browser
                            let answer = peer_connection.create_answer(None).await.unwrap();

                            // Sets the LocalDescription, and starts our UDP listeners
                            peer_connection.set_local_description(answer).await.unwrap();

                            // Create channel that is blocked until ICE Gathering is complete
                            let mut gather_complete =
                                peer_connection.gathering_complete_promise().await;

                            // Block until ICE Gathering is complete, disabling trickle ICE
                            // we do this because we only can exchange one signaling message
                            // in a production application you should exchange ICE Candidates via OnICECandidate
                            let _ = gather_complete.recv().await;

                            println!("Connection established, waiting for messages...");

                            PEER_CONNECTION.set(peer_connection).unwrap();
                        }
                        Err(e) => {
                            eprintln!("websocket handshake failed: {e}");
                            eprintln!(
                                "Hint: Ensure your client connects with wss://localhost:8002 and trusts the local certificate in certs/."
                            );
                        }
                    }
                });
            }
            Err(e) => return Err(anyhow::anyhow!("TCP accept error: {e}")),
        }
    }
    Ok(())
}
