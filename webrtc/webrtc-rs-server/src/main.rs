//! Example websocket server.
//!
//! Run the server with
//! ```not_rust
//! cargo run -p example-websockets --bin example-websockets
//! ```
//!
//! Run a browser client with
//! ```not_rust
//! firefox http://localhost:3000
//! ```
//!
//! Alternatively you can run the rust client (showing two
//! concurrent websocket connections being established) with
//! ```not_rust
//! cargo run -p example-websockets --bin example-client
//! ```

use axum::{
    Router,
    body::Bytes,
    extract::ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade},
    response::IntoResponse,
    routing::any,
};
use axum_extra::TypedHeader;
use rustls::ServerConfig as RustlsServerConfig;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use webrtc::{
    api::{
        APIBuilder, interceptor_registry::register_default_interceptors, media_engine::MediaEngine,
    },
    data_channel::RTCDataChannel,
    ice_transport::{ice_candidate::RTCIceCandidate, ice_server::RTCIceServer},
    peer_connection::{
        RTCPeerConnection, configuration::RTCConfiguration,
        peer_connection_state::RTCPeerConnectionState,
        sdp::session_description::RTCSessionDescription,
    },
};

use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, OnceLock},
};
use std::{ops::ControlFlow, path::Path};
use tower_http::{
    services::ServeDir,
    trace::{DefaultMakeSpan, TraceLayer},
};

use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

//allows to extract the IP of connecting user
use axum::extract::connect_info::ConnectInfo;
use axum::extract::ws::CloseFrame;
use axum_extra::headers;

//allows to split the websocket stream into separate TX and RX branches
use futures_util::{sink::SinkExt, stream::StreamExt};

static PEER_CONNECTION: OnceLock<Arc<RTCPeerConnection>> = OnceLock::new();

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                format!("{}=debug,tower_http=debug", env!("CARGO_CRATE_NAME")).into()
            }),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    CryptoProvider::install_default(rustls::crypto::ring::default_provider()).unwrap();

    let assets_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets");

    // Use fixed relative paths from this crate to the repo certs
    let cert_path = Path::new("../../certs/localhost.pem");
    let key_path = Path::new("../../certs/localhost-key.pem");

    // build our application with some routes
    let app = Router::new()
        .fallback_service(ServeDir::new(assets_dir).append_index_html_on_directories(true))
        .route("/", any(ws_handler))
        // logging so we can see what's going on
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::default().include_headers(true)),
        );
    // run it with hyper
    let bind_addr = SocketAddr::from(([127, 0, 0, 1], 8002));
    tracing::debug!("listening on {}", bind_addr);
    // Build a Rustls ServerConfig and force ALPN to http/1.1 so WebSocket upgrade works over TLS
    let cert = CertificateDer::from_pem_file(cert_path).expect("load certs");
    let key = PrivateKeyDer::from_pem_file(key_path).expect("load private key");
    let mut tls_config = RustlsServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .expect("invalid cert/key");
    tls_config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let tls_config =
        axum_server::tls_rustls::RustlsConfig::from_config(std::sync::Arc::new(tls_config));
    let server = axum_server::bind_rustls(bind_addr, tls_config);
    // Provide peer SocketAddr to handlers so ConnectInfo extractor works (prevents 500s on WS upgrade)
    server
        .serve(app.into_make_service_with_connect_info::<SocketAddr>())
        .await
        .unwrap();
}

/// The handler for the HTTP request (this gets called when the HTTP request lands at the start
/// of websocket negotiation). After this completes, the actual switching from HTTP to
/// websocket protocol will occur.
/// This is the last point where we can extract TCP/IP metadata such as IP address of the client
/// as well as things from HTTP headers such as user-agent of the browser etc.
async fn ws_handler(
    ws: WebSocketUpgrade,
    user_agent: Option<TypedHeader<headers::UserAgent>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> impl IntoResponse {
    let user_agent = if let Some(TypedHeader(user_agent)) = user_agent {
        user_agent.to_string()
    } else {
        String::from("Unknown browser")
    };
    println!("`{user_agent}` at {addr} connected.");
    // finalize the upgrade process by returning upgrade callback.
    // we can customize the callback by sending additional info such as address.
    ws.on_upgrade(move |socket| handle_socket(socket, addr))
}

/// Actual websocket statemachine (one will be spawned per connection)
async fn handle_socket(mut socket: WebSocket, who: SocketAddr) {
    // send a ping (unsupported by some browsers) just to kick things off and get a response
    if socket
        .send(Message::Ping(Bytes::from_static(&[1, 2, 3])))
        .await
        .is_ok()
    {
        println!("Pinged {who}...");
    } else {
        println!("Could not send ping {who}!");
        // no Error here since the only thing we can do is to close the connection.
        // If we can not send messages, there is no way to salvage the statemachine anyway.
        return;
    }

    // Create a MediaEngine object to configure the supported codec
    let mut m = MediaEngine::default();

    // Register default codecs
    m.register_default_codecs().unwrap();

    // Create a InterceptorRegistry. This is the user configurable RTP/RTCP Pipeline.
    // This provides NACKs, RTCP Reports and other features. If you use `webrtc.NewPeerConnection`
    // this is enabled by default. If you are manually managing You MUST create a InterceptorRegistry
    // for each PeerConnection.
    let mut registry = interceptor::registry::Registry::new();

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
    let peer_connection = Arc::new(api.new_peer_connection(config).await.unwrap());

    // Set the handler for Peer connection state
    // This will notify you when the peer has connected/disconnected
    peer_connection.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
        println!("Peer Connection State has changed: {s}");

        if s == RTCPeerConnectionState::Failed {
            // Wait until PeerConnection has had no network activity for 30 seconds or another failure. It may be reconnected using an ICE Restart.
            // Use webrtc.PeerConnectionStateDisconnected if you are interested in detecting faster timeout.
            // Note that the PeerConnection may come back from PeerConnectionStateDisconnected.
            println!("Peer Connection has gone to failed exiting");
        }

        Box::pin(async {})
    }));

    peer_connection.on_ice_candidate(Box::new(move |candidate: Option<RTCIceCandidate>| {
        Box::pin(async move {
            if let Some(c) = candidate {
                println!("New ICE candidate: {}", c.to_json().unwrap().candidate);
            } else {
                println!("ICE gathering complete");
            }
        })
    }));

    peer_connection.on_data_channel(Box::new(move |data_channel: Arc<RTCDataChannel>| {
        println!(
            "New DataChannel {}-{}",
            data_channel.label(),
            data_channel.id()
        );
        Box::pin(async move {})
    }));

    while let Some(msg) = socket.recv().await {
        if let Ok(msg) = msg {
            let sdp_bytes = msg.into_text().expect("msg to text");
            println!("Received SDP offer from client {sdp_bytes}");
            if let Some(offer) = serde_json::from_str::<RTCSessionDescription>(&sdp_bytes).ok() {
                println!("Received offer: {offer:?}");

                let pc = peer_connection.clone();

                // Apply the offer as the remote description
                pc.set_remote_description(offer).await.unwrap();

                // Create an answer to send to the browser
                let answer = pc.create_answer(None).await.unwrap();

                // Sets the LocalDescription, and starts our UDP listeners
                pc.set_local_description(answer).await.unwrap();

                // Create channel that is blocked until ICE Gathering is complete
                let mut gather_complete = pc.gathering_complete_promise().await;

                // Block until ICE Gathering is complete, disabling trickle ICE
                // we do this because we only can exchange one signaling message
                // in a production application you should exchange ICE Candidates via OnICECandidate
                let _ = gather_complete.recv().await;

                println!("Connection established, waiting for messages...");

                PEER_CONNECTION.set(pc).unwrap();
                return;
            } else {
                println!("Could not parse SDP from client {who}");
                continue;
            }
        } else {
            println!("client {who} abruptly disconnected");
            return;
        }
    }

    // returning from the handler closes the websocket connection
    println!("Websocket context {who} destroyed");
}
