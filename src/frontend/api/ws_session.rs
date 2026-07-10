use std::sync::Arc;

use axum::{
    extract::{
        Path, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::IntoResponse,
    Extension,
};
use tokio::sync::broadcast;
use tracing::{info, warn};

use skald_core::skald::Skald;

use super::guard::AuthUser;

pub async fn handler(
    ws:              WebSocketUpgrade,
    Path(id):        Path<i64>,
    Extension(auth): Extension<AuthUser>,
    State(skald):    State<Arc<Skald>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, skald, id, auth.user_id))
}

async fn handle_socket(mut socket: WebSocket, skald: Arc<Skald>, session_id: i64, user_id: String) {
    info!(session_id, user = %user_id, "session-watch WS connected");

    // Watch this user's own event stream, so a session-watch only ever sees events
    // for sessions in the watcher's `{userid}.db`.
    let ctx = match skald.user_context(&user_id).await {
        Some(c) => c,
        None => return,
    };
    let mut rx = ctx.chat_hub.events("session-watch");

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
