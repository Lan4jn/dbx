use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, OnceLock};

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Query, State, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;

use dbx_core::connection::AppState;

const DEFAULT_PUBSUB_PORT: u16 = 4224;
static ACTUAL_PUBSUB_PORT: OnceLock<AtomicU16> = OnceLock::new();

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PubSubWsParams {
    connection_id: String,
}

pub fn build_pubsub_router(state: Arc<AppState>) -> Router {
    Router::new().route("/api/redis/pubsub/ws", get(ws_handler)).with_state(state)
}

fn pubsub_server_port() -> u16 {
    std::env::var("DBX_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(DEFAULT_PUBSUB_PORT)
}

#[tauri::command]
pub fn redis_pubsub_server_port() -> u16 {
    actual_pubsub_port().unwrap_or_else(pubsub_server_port)
}

fn actual_pubsub_port() -> Option<u16> {
    let port = ACTUAL_PUBSUB_PORT.get().map(|value| value.load(Ordering::Relaxed)).unwrap_or(0);
    (port > 0).then_some(port)
}

fn set_actual_pubsub_port(port: u16) {
    ACTUAL_PUBSUB_PORT.get_or_init(|| AtomicU16::new(0)).store(port, Ordering::Relaxed);
}

fn pubsub_bind_ports(requested: u16) -> Vec<u16> {
    if requested == 0 {
        vec![0]
    } else {
        vec![requested, 0]
    }
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(params): Query<PubSubWsParams>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let connection_id = params.connection_id;
    ws.on_upgrade(move |socket| handle_socket(socket, state, connection_id))
}

async fn handle_socket(socket: WebSocket, state: Arc<AppState>, connection_id: String) {
    // Create PubSub connection
    let pubsub = match dbx_core::redis_ops::redis_create_pubsub_core(&state, &connection_id).await {
        Ok(p) => p,
        Err(e) => {
            let (mut sender, _) = socket.split();
            let _ = sender.send(Message::Text(format!(r#"{{"error":"{e}"}}"#).into())).await;
            return;
        }
    };

    let (mut sink, mut stream) = pubsub.split();
    let (mut ws_sender, mut ws_receiver) = socket.split();

    // Channel for WebSocket commands -> PubSub sink
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    // Task: Read WebSocket commands
    let ws_read = tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_receiver.next().await {
            match msg {
                Message::Text(text) => {
                    if cmd_tx.send(text.to_string()).is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    });

    // Task: Apply commands to PubSub sink
    let sink_handle = tokio::spawn(async move {
        while let Some(text) = cmd_rx.recv().await {
            if let Err(e) = handle_command(&mut sink, &text).await {
                log::warn!("PubSub command error: {e}");
            }
        }
    });

    // Forward Redis messages to WebSocket (uses ws_sender, no mutex contention)
    while let Some(msg) = stream.next().await {
        let payload: String = msg.get_payload().unwrap_or_default();
        let channel = msg.get_channel_name().to_string();
        let pattern: Option<String> = msg.get_pattern().ok();
        let json = serde_json::json!({
            "channel": channel,
            "pattern": pattern,
            "payload": payload,
        });
        let text = serde_json::to_string(&json).unwrap_or_default();
        if ws_sender.send(Message::Text(text.into())).await.is_err() {
            break;
        }
    }

    ws_read.abort();
    sink_handle.abort();
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum PubSubCommand {
    #[serde(rename = "subscribe")]
    Subscribe { channels: Vec<String> },
    #[serde(rename = "psubscribe")]
    Psubscribe { patterns: Vec<String> },
    #[serde(rename = "unsubscribe")]
    Unsubscribe { channels: Vec<String> },
    #[serde(rename = "punsubscribe")]
    Punsubscribe { patterns: Vec<String> },
}

async fn handle_command(sink: &mut redis::aio::PubSubSink, text: &str) -> Result<(), String> {
    let cmd: PubSubCommand = serde_json::from_str(text).map_err(|e| format!("Invalid PubSub command: {e}"))?;

    match cmd {
        PubSubCommand::Subscribe { channels } => {
            for ch in &channels {
                sink.subscribe(ch).await.map_err(|e| format!("Subscribe error: {e}"))?;
            }
        }
        PubSubCommand::Psubscribe { patterns } => {
            for pat in &patterns {
                sink.psubscribe(pat).await.map_err(|e| format!("PSubscribe error: {e}"))?;
            }
        }
        PubSubCommand::Unsubscribe { channels } => {
            for ch in &channels {
                sink.unsubscribe(ch).await.map_err(|e| format!("Unsubscribe error: {e}"))?;
            }
        }
        PubSubCommand::Punsubscribe { patterns } => {
            for pat in &patterns {
                sink.punsubscribe(pat).await.map_err(|e| format!("PUnsubscribe error: {e}"))?;
            }
        }
    }
    Ok(())
}

/// Start the embedded web server for PubSub WebSocket support.
/// Runs on a background task using the shared AppState.
pub fn start_pubsub_server(state: Arc<AppState>) {
    let router = build_pubsub_router(state);
    tauri::async_runtime::spawn(async move {
        let requested_port = pubsub_server_port();
        let mut listener = None;
        for port in pubsub_bind_ports(requested_port) {
            let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
            match tokio::net::TcpListener::bind(addr).await {
                Ok(bound) => {
                    listener = Some(bound);
                    break;
                }
                Err(error) => {
                    log::warn!("Failed to bind PubSub server on {addr}: {error}");
                }
            }
        };
        let Some(listener) = listener else {
            log::warn!("Failed to bind PubSub server on requested port {requested_port} and fallback port");
            return;
        };
        let addr = listener.local_addr().ok();
        if let Some(addr) = addr {
            set_actual_pubsub_port(addr.port());
            log::info!("PubSub WebSocket server listening on {addr}");
        }
        if let Err(error) = axum::serve(listener, router).await {
            log::warn!("PubSub server stopped with error: {error}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::pubsub_bind_ports;

    #[test]
    fn pubsub_bind_ports_fall_back_to_ephemeral_for_default_port() {
        assert_eq!(pubsub_bind_ports(4224), vec![4224, 0]);
    }

    #[test]
    fn pubsub_bind_ports_do_not_duplicate_ephemeral_request() {
        assert_eq!(pubsub_bind_ports(0), vec![0]);
    }
}
