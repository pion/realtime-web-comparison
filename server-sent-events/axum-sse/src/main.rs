use async_stream::stream;
use axum::{
    Router,
    extract::State,
    response::sse::{Event, Sse},
    routing::get,
};
use std::convert::Infallible;

use futures::stream::Stream;
use tokio::sync::broadcast;
use tokio::sync::broadcast::Sender;

#[tokio::main]
async fn main() {
    let (tx, _rx) = broadcast::channel::<String>(100);
    let app = Router::new().route("/", get(sse_handler)).with_state(tx);
    let listener = tokio::net::TcpListener::bind("0.0.0.0:7999").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn sse_handler(
    State(tx): State<Sender<String>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // broadcast::Receiver
    let mut rx = tx.subscribe();
    Sse::new(stream! {
         // Send message so we can test the server performance
            for i in (10..=510).step_by(10) {
                for j in (10..=510).step_by(10) {
                    let message = format!("{j},{i}");
                    yield Ok(Event::default().data::<String>(message));
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
        println!("All messages sent.");
    })
}
