use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use rustls::ClientConfig;
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{interval_at, sleep_until, Instant, MissedTickBehavior};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async_tls_with_config, Connector, MaybeTlsStream, WebSocketStream};

use crate::config::EdgeConfig;
use crate::protocol::{
    decode_control_frame, encode_control_frame, EdgeRegistration, EdgeToMain, MainToEdge, ProtocolVersion,
    RegisteredTarget, MAX_CONTROL_FRAME_SIZE,
};
use crate::tls::{load_certificates, load_private_key, load_roots};
use crate::{GatewayError, GatewayErrorCode};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CONTROL_IO_TIMEOUT: Duration = Duration::from_secs(1);

pub struct EdgeGateway {
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl EdgeGateway {
    pub fn start(config: EdgeConfig) -> Result<Self, GatewayError> {
        let tls = Arc::new(
            ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
                .with_root_certificates(load_roots(&config.ca_certificate)?)
                .with_client_auth_cert(load_certificates(&config.certificate)?, load_private_key(&config.private_key)?)
                .map_err(|_| tls_error("Edge certificate or private key was rejected"))?,
        );
        let (shutdown, stop) = watch::channel(false);
        let task = tokio::spawn(run(config, tls, stop));
        Ok(Self { shutdown, task })
    }

    pub async fn shutdown(mut self) {
        let _ = self.shutdown.send(true);
        let _ = (&mut self.task).await;
    }
}

impl Drop for EdgeGateway {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
    }
}

#[derive(Clone, Debug)]
pub struct ReconnectBackoff {
    next_seconds: u64,
}

impl ReconnectBackoff {
    pub fn new() -> Self {
        Self { next_seconds: 1 }
    }

    pub fn next_delay(&mut self, jitter_percent: u64) -> Duration {
        let seconds = self.next_seconds;
        self.next_seconds = (self.next_seconds * 2).min(60);
        Duration::from_millis(seconds * 1_000 * (100 + jitter_percent.min(20)) / 100)
    }

    pub fn reset(&mut self) {
        self.next_seconds = 1;
    }

    fn record_registration(&mut self, confirmed: bool) {
        if confirmed {
            self.reset();
        }
    }
}

impl Default for ReconnectBackoff {
    fn default() -> Self {
        Self::new()
    }
}

async fn run(config: EdgeConfig, tls: Arc<ClientConfig>, mut stop: watch::Receiver<bool>) {
    let mut backoff = ReconnectBackoff::new();
    loop {
        if *stop.borrow() {
            return;
        }
        let connected = tokio::select! {
            result = tokio::time::timeout(CONNECT_TIMEOUT, connect(&config, tls.clone())) => result,
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    return;
                }
                continue;
            }
        };
        let retry_from = if let Ok(Ok(socket)) = connected {
            let (registered, disconnected_at) = run_control_session(socket, &config, stop.clone()).await;
            backoff.record_registration(registered);
            disconnected_at
        } else {
            Instant::now()
        };
        let delay = backoff.next_delay(jitter_percent());
        tokio::select! {
            _ = sleep_until(retry_from + delay) => {}
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    return;
                }
            }
        }
    }
}

async fn connect(
    config: &EdgeConfig,
    tls: Arc<ClientConfig>,
) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, GatewayError> {
    connect_async_tls_with_config(
        &config.main_url,
        Some(control_websocket_config()),
        false,
        Some(Connector::Rustls(tls)),
    )
    .await
    .map(|(socket, _)| socket)
    .map_err(|_| internal_error("Edge control connection failed"))
}

pub(crate) fn control_websocket_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .read_buffer_size(16 * 1024)
        .write_buffer_size(16 * 1024)
        .max_write_buffer_size(MAX_CONTROL_FRAME_SIZE * 2)
        .max_message_size(Some(MAX_CONTROL_FRAME_SIZE))
        .max_frame_size(Some(MAX_CONTROL_FRAME_SIZE))
}

async fn run_control_session(
    mut socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    config: &EdgeConfig,
    mut stop: watch::Receiver<bool>,
) -> (bool, Instant) {
    let registration = EdgeRegistration {
        version: ProtocolVersion::current(),
        edge_id: config.edge_id.clone(),
        targets: config
            .targets
            .iter()
            .map(|(target_id, target)| RegisteredTarget {
                target_id: target_id.clone(),
                display_name: target.display_name.clone(),
            })
            .collect(),
    };
    let Ok(frame) = encode_control_frame(&registration) else {
        return (false, Instant::now());
    };
    if !send_control_message(&mut socket, Message::Binary(frame.into())).await {
        return (false, Instant::now());
    }
    let heartbeat_message = EdgeToMain::Heartbeat { version: ProtocolVersion::current() };
    let Ok(frame) = encode_control_frame(&heartbeat_message) else {
        return (false, Instant::now());
    };
    if !send_control_message(&mut socket, Message::Binary(frame.into())).await {
        return (false, Instant::now());
    }

    let mut heartbeat = interval_at(Instant::now() + HEARTBEAT_INTERVAL, HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut registered = false;
    loop {
        tokio::select! {
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    let disconnected_at = Instant::now();
                    close_control_socket(&mut socket).await;
                    return (registered, disconnected_at);
                }
            }
            _ = heartbeat.tick() => {
                let message = EdgeToMain::Heartbeat { version: ProtocolVersion::current() };
                let Ok(frame) = encode_control_frame(&message) else { return (registered, Instant::now()) };
                if !send_control_message(&mut socket, Message::Binary(frame.into())).await {
                    return (registered, Instant::now());
                }
            }
            incoming = socket.next() => {
                let Some(Ok(message)) = incoming else { return (registered, Instant::now()) };
                match message {
                    Message::Binary(frame) => match decode_control_frame::<MainToEdge>(&frame) {
                        Ok(MainToEdge::HeartbeatAck { .. }) => registered = true,
                        Ok(MainToEdge::OpenDataChannel { .. }) if registered => {}
                        _ => return (registered, Instant::now()),
                    },
                    Message::Ping(data) => {
                        if !send_control_message(&mut socket, Message::Pong(data)).await {
                            return (registered, Instant::now());
                        }
                    }
                    Message::Close(_) => {
                        let disconnected_at = Instant::now();
                        close_control_socket(&mut socket).await;
                        return (registered, disconnected_at);
                    }
                    _ => return (registered, Instant::now()),
                }
            }
        }
    }
}

async fn send_control_message(socket: &mut WebSocketStream<MaybeTlsStream<TcpStream>>, message: Message) -> bool {
    matches!(tokio::time::timeout(CONTROL_IO_TIMEOUT, socket.send(message)).await, Ok(Ok(())))
}

async fn close_control_socket(socket: &mut WebSocketStream<MaybeTlsStream<TcpStream>>) {
    let _ = tokio::time::timeout(CONTROL_IO_TIMEOUT, socket.close(None)).await;
}

fn jitter_percent() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::from(duration.subsec_nanos()) % 21)
        .unwrap_or_default()
}

fn tls_error(message: &str) -> GatewayError {
    GatewayError { code: GatewayErrorCode::TlsRejected, message: message.to_string() }
}

fn internal_error(message: &str) -> GatewayError {
    GatewayError { code: GatewayErrorCode::Internal, message: message.to_string() }
}

#[cfg(test)]
mod tests {
    use super::control_websocket_config;
    use crate::protocol::MAX_CONTROL_FRAME_SIZE;

    #[test]
    fn control_websocket_is_bounded_to_the_protocol_frame_limit() {
        let config = control_websocket_config();

        assert_eq!(config.max_message_size, Some(MAX_CONTROL_FRAME_SIZE));
        assert_eq!(config.max_frame_size, Some(MAX_CONTROL_FRAME_SIZE));
        assert!(config.read_buffer_size <= MAX_CONTROL_FRAME_SIZE);
        assert!(config.max_write_buffer_size <= MAX_CONTROL_FRAME_SIZE * 2);
    }

    #[test]
    fn reconnect_backoff_resets_only_after_registration_is_confirmed() {
        let mut backoff = super::ReconnectBackoff::new();

        assert_eq!(backoff.next_delay(0), std::time::Duration::from_secs(1));
        backoff.record_registration(false);
        assert_eq!(backoff.next_delay(0), std::time::Duration::from_secs(2));
        backoff.record_registration(true);
        assert_eq!(backoff.next_delay(0), std::time::Duration::from_secs(1));
    }
}
