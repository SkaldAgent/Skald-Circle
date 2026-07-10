use std::sync::Arc;

use axum::{
    extract::{
        Path, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::IntoResponse,
};
use tokio::sync::broadcast;
use tracing::{info, warn};

use skald_core::skald::Skald;

pub async fn handler(
    ws:           WebSocketUpgrade,
    Path(id):     Path<i64>,
    State(skald): State<Arc<Skald>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, skald, id))
}

async fn handle_socket(mut socket: WebSocket, skald: Arc<Skald>, session_id: i64) {
    info!(session_id, "session-watch WS connected");

    let mut rx = skald.chat_hub().events("session-watch");

    loop {
        tokio::select! {
            // Detect client disconnect.
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {} // ignore any inbound data
                }
            }

            // Forward bus events filtered by session_id.
            event = rx.recv() => {
                match event {
                    Ok(ge) => {
                        if ge.session_id != Some(session_id) {
                            continue;
                        }
                        let text = ge.event.to_json();
                        if socket.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(session_id, skipped = n, "session-watch WS: event bus lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    info!(session_id, "session-watch WS disconnected");
}
