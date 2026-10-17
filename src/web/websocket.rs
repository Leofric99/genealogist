use crate::{queue, state::AppState};
use axum::{
    extract::{
        ws::{Message, WebSocket},
        State, WebSocketUpgrade,
    },
    response::Response,
};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;

pub async fn handler(State(state): State<Arc<AppState>>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |socket| stream_progress(socket, state))
}

async fn stream_progress(mut socket: WebSocket, state: Arc<AppState>) {
    if let Ok(status) = queue::progress_snapshot(&state) {
        if let Ok(payload) = serde_json::to_string(&status) {
            if socket.send(Message::Text(payload.into())).await.is_err() {
                return;
            }
        }
    }
    let mut receiver = state.progress_tx.subscribe();
    loop {
        tokio::select! {
            update = receiver.recv() => match update {
                Ok(message) => {
                    if socket.send(Message::Text(message.into())).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    if let Ok(status) = queue::progress_snapshot(&state) {
                        if let Ok(payload) = serde_json::to_string(&status) {
                            if socket.send(Message::Text(payload.into())).await.is_err() {
                                break;
                            }
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            incoming = socket.next() => match incoming {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(Message::Ping(bytes))) => {
                    if socket.send(Message::Pong(bytes)).await.is_err() {
                        break;
                    }
                }
                _ => {}
            }
        }
    }
}
