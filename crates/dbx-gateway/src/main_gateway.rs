use std::collections::{BTreeMap, HashMap};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use http_body_util::Empty;
use hyper::body::{Bytes, Incoming};
use hyper::header::{CONNECTION, SEC_WEBSOCKET_ACCEPT, SEC_WEBSOCKET_KEY, SEC_WEBSOCKET_VERSION, UPGRADE};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode, Version};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch, OwnedSemaphorePermit, RwLock, RwLockWriteGuard, Semaphore};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{sleep, timeout, Instant};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use uuid::Uuid;

use crate::config::MainConfig;
use crate::edge_gateway::control_websocket_config;
use crate::protocol::{
    decode_edge_message, decode_edge_registration, encode_control_frame, EdgeToMain, MainToEdge, RegisteredTarget,
};
use crate::tls::{GatewayTls, PeerIdentity};
use crate::{GatewayError, GatewayErrorCode};

const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(45);
const CONTROL_IO_TIMEOUT: Duration = Duration::from_secs(1);

pub type EdgeRegistry = Arc<RwLock<HashMap<String, EdgeEntry>>>;

#[derive(Clone, Debug)]
pub struct EdgeIdentity {
    pub edge_id: String,
    pub serial: String,
    pub fingerprint_sha256: [u8; 32],
}

#[derive(Clone, Debug)]
pub struct EdgeEntry {
    pub identity: EdgeIdentity,
    pub targets: BTreeMap<String, RegisteredTarget>,
    pub last_heartbeat: Instant,
    pub control_tx: mpsc::Sender<MainToEdge>,
    pub active_streams: usize,
    pub connection_id: Uuid,
    pub online: bool,
}

pub struct MainGateway {
    local_addr: SocketAddr,
    registry: EdgeRegistry,
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl MainGateway {
    pub async fn bind(config: MainConfig) -> Result<Self, GatewayError> {
        let tls = Arc::new(GatewayTls::load(&config)?);
        let listener =
            TcpListener::bind(&config.listen).await.map_err(|_| internal_error("main listener could not bind"))?;
        let local_addr = listener.local_addr().map_err(|_| internal_error("main listener address unavailable"))?;
        let acceptor = TlsAcceptor::from(tls.server_config.clone());
        let edge_path = Arc::<str>::from(config.edge_path);
        let dbx_path = Arc::<str>::from(config.dbx_path);
        let registry = EdgeRegistry::default();
        let task_registry = registry.clone();
        let control_tasks = Arc::new(Mutex::new(Vec::<JoinHandle<()>>::new()));
        let task_control_tasks = control_tasks.clone();
        let connection_slots = Arc::new(Semaphore::new(config.max_connections));
        let tls_handshake_timeout = Duration::from_secs(config.tls_handshake_timeout_secs);
        let http_header_timeout = Duration::from_secs(config.http_header_timeout_secs);
        let (shutdown, mut stop) = watch::channel(false);
        let task_shutdown = shutdown.clone();
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                let accepted = tokio::select! {
                    changed = stop.changed() => {
                        if changed.is_err() || *stop.borrow() {
                            break;
                        }
                        continue;
                    }
                    completed = connections.join_next(), if !connections.is_empty() => {
                        let _ = completed;
                        continue;
                    }
                    result = listener.accept() => result,
                };
                let stream = match accepted {
                    Ok((stream, _)) => stream,
                    Err(_) => {
                        sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let Ok(permit) = connection_slots.clone().try_acquire_owned() else {
                    continue;
                };
                let acceptor = acceptor.clone();
                let tls = tls.clone();
                let edge_path = edge_path.clone();
                let dbx_path = dbx_path.clone();
                let registry = task_registry.clone();
                let control_tasks = task_control_tasks.clone();
                let connection_stop = task_shutdown.subscribe();
                connections.spawn(async move {
                    let Ok(Ok(stream)) = timeout(tls_handshake_timeout, acceptor.accept(stream)).await else {
                        return;
                    };
                    let Ok(identity) = tls.classify(stream.get_ref().1.peer_certificates()) else {
                        return;
                    };
                    let connection_permit = Arc::new(Mutex::new(Some(permit)));
                    let (first_request_tx, first_request_rx) = tokio::sync::oneshot::channel();
                    let first_request_tx = Arc::new(Mutex::new(Some(first_request_tx)));
                    let service = service_fn(move |mut request: Request<Incoming>| {
                        if let Ok(mut sender) = first_request_tx.lock() {
                            if let Some(sender) = sender.take() {
                                let _ = sender.send(());
                            }
                        }
                        request.extensions_mut().insert(identity.clone());
                        route(
                            request,
                            edge_path.clone(),
                            dbx_path.clone(),
                            registry.clone(),
                            control_tasks.clone(),
                            connection_permit.clone(),
                            connection_stop.clone(),
                        )
                    });
                    let mut builder = auto::Builder::new(TokioExecutor::new());
                    builder.http1().timer(TokioTimer::new()).header_read_timeout(http_header_timeout);
                    let connection = builder.serve_connection_with_upgrades(TokioIo::new(stream), service);
                    tokio::pin!(connection);
                    tokio::select! {
                        _ = &mut connection => {}
                        first = timeout(http_header_timeout, first_request_rx) => {
                            if first.is_ok() {
                                connection.as_mut().graceful_shutdown();
                                let _ = connection.await;
                            }
                        }
                    }
                });
            }
            connections.abort_all();
            while connections.join_next().await.is_some() {}
            let controls = task_control_tasks.lock().map(|mut tasks| std::mem::take(&mut *tasks)).unwrap_or_default();
            for control in controls {
                let _ = control.await;
            }
        });
        Ok(Self { local_addr, registry, shutdown, task })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn registry(&self) -> EdgeRegistry {
        self.registry.clone()
    }

    pub async fn shutdown(mut self) {
        let _ = self.shutdown.send(true);
        let _ = (&mut self.task).await;
    }
}

impl Drop for MainGateway {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
    }
}

async fn route(
    mut request: Request<Incoming>,
    edge_path: Arc<str>,
    dbx_path: Arc<str>,
    registry: EdgeRegistry,
    control_tasks: Arc<Mutex<Vec<JoinHandle<()>>>>,
    connection_permit: Arc<Mutex<Option<OwnedSemaphorePermit>>>,
    stop: watch::Receiver<bool>,
) -> Result<Response<Empty<Bytes>>, Infallible> {
    let identity = request.extensions().get::<PeerIdentity>().cloned();
    if let Some(PeerIdentity::Edge { edge_id, serial, fingerprint_sha256 }) = identity {
        if request.uri().path() == edge_path.as_ref() {
            if let Some(accept_key) = websocket_accept_key(&request) {
                let Some(permit) = connection_permit.lock().ok().and_then(|mut permit| permit.take()) else {
                    return Ok(empty_response(StatusCode::NOT_FOUND));
                };
                let version = request.version();
                let control = tokio::spawn(async move {
                    let _permit = permit;
                    let Ok(upgraded) = hyper::upgrade::on(&mut request).await else {
                        return;
                    };
                    let socket = WebSocketStream::from_raw_socket(
                        TokioIo::new(upgraded),
                        Role::Server,
                        Some(control_websocket_config()),
                    )
                    .await;
                    run_edge_control(socket, edge_id, serial, fingerprint_sha256, registry, stop).await;
                });
                if let Ok(mut tasks) = control_tasks.lock() {
                    tasks.retain(|task| !task.is_finished());
                    tasks.push(control);
                }
                return Ok(switching_protocols(version, accept_key));
            }
            return Ok(empty_response(StatusCode::OK));
        }
        return Ok(empty_response(StatusCode::NOT_FOUND));
    }
    if matches!(identity, Some(PeerIdentity::DbxClient { .. })) && request.uri().path() == dbx_path.as_ref() {
        return Ok(empty_response(StatusCode::OK));
    }
    Ok(empty_response(StatusCode::NOT_FOUND))
}

fn websocket_accept_key(request: &Request<Incoming>) -> Option<String> {
    let headers = request.headers();
    let key = headers.get(SEC_WEBSOCKET_KEY)?;
    let has_upgrade =
        headers.get(CONNECTION)?.to_str().ok()?.split(',').any(|token| token.trim().eq_ignore_ascii_case("upgrade"));
    (request.method() == Method::GET
        && request.version() >= Version::HTTP_11
        && has_upgrade
        && headers.get(UPGRADE)?.to_str().ok()?.eq_ignore_ascii_case("websocket")
        && headers.get(SEC_WEBSOCKET_VERSION)? == "13")
        .then(|| derive_accept_key(key.as_bytes()))
}

fn switching_protocols(version: Version, accept_key: String) -> Response<Empty<Bytes>> {
    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .version(version)
        .header(CONNECTION, "Upgrade")
        .header(UPGRADE, "websocket")
        .header(SEC_WEBSOCKET_ACCEPT, accept_key)
        .body(Empty::new())
        .expect("fixed WebSocket response is valid")
}

fn empty_response(status: StatusCode) -> Response<Empty<Bytes>> {
    Response::builder().status(status).body(Empty::new()).expect("fixed response is valid")
}

async fn run_edge_control<S>(
    mut socket: WebSocketStream<S>,
    certificate_edge_id: String,
    serial: String,
    fingerprint_sha256: [u8; 32],
    registry: EdgeRegistry,
    mut stop: watch::Receiver<bool>,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let incoming = tokio::select! {
        incoming = timeout(HEARTBEAT_TIMEOUT, socket.next()) => incoming,
        _ = wait_for_stop(&mut stop) => {
            close_control_socket(&mut socket).await;
            return;
        }
    };
    let Ok(Some(Ok(Message::Binary(frame)))) = incoming else {
        close_control_socket(&mut socket).await;
        return;
    };
    let Ok(registration) = decode_edge_registration(&frame) else {
        close_control_socket(&mut socket).await;
        return;
    };
    if registration.edge_id != certificate_edge_id {
        close_control_socket(&mut socket).await;
        return;
    }
    let Some(targets) = registration_targets(registration.targets) else {
        close_control_socket(&mut socket).await;
        return;
    };

    let connection_id = Uuid::new_v4();
    let (control_tx, mut control_rx) = mpsc::channel(32);
    let duplicate = {
        let Some(mut entries) = registry_write_until_stop(&registry, &mut stop).await else {
            close_control_socket(&mut socket).await;
            return;
        };
        if entries.get(&certificate_edge_id).is_some_and(|entry| entry.online) {
            true
        } else {
            entries.insert(
                certificate_edge_id.clone(),
                EdgeEntry {
                    identity: EdgeIdentity { edge_id: certificate_edge_id.clone(), serial, fingerprint_sha256 },
                    targets,
                    last_heartbeat: Instant::now(),
                    control_tx,
                    active_streams: 0,
                    connection_id,
                    online: true,
                },
            );
            false
        }
    };
    if duplicate {
        close_control_socket(&mut socket).await;
        return;
    }

    let heartbeat_deadline = tokio::time::sleep(HEARTBEAT_TIMEOUT);
    tokio::pin!(heartbeat_deadline);
    loop {
        tokio::select! {
            _ = &mut heartbeat_deadline => {
                close_control_socket(&mut socket).await;
                break;
            }
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    close_control_socket(&mut socket).await;
                    let _ = timeout(Duration::from_secs(1), socket.next()).await;
                    break;
                }
            }
            outgoing = control_rx.recv() => {
                let Some(outgoing) = outgoing else { break };
                let Ok(frame) = encode_control_frame(&outgoing) else { break };
                if !send_control_message(&mut socket, Message::Binary(frame.into())).await {
                    break;
                }
            }
            incoming = socket.next() => {
                let Some(Ok(message)) = incoming else { break };
                match message {
                    Message::Binary(frame) => {
                        let Ok(EdgeToMain::Heartbeat { .. }) = decode_edge_message(&frame) else { break };
                        let now = Instant::now();
                        let Some(mut entries) = registry_write_until_stop(&registry, &mut stop).await else {
                            close_control_socket(&mut socket).await;
                            break;
                        };
                        let Some(entry) = entries.get_mut(&certificate_edge_id).filter(|entry| entry.connection_id == connection_id) else {
                            break;
                        };
                        entry.last_heartbeat = now;
                        drop(entries);
                        heartbeat_deadline.as_mut().reset(now + HEARTBEAT_TIMEOUT);
                        let Ok(frame) = encode_control_frame(&MainToEdge::HeartbeatAck { unix_ms: unix_time_ms() }) else { break };
                        if !send_control_message(&mut socket, Message::Binary(frame.into())).await {
                            break;
                        }
                    }
                    Message::Ping(data) => {
                        if !send_control_message(&mut socket, Message::Pong(data)).await {
                            break;
                        }
                    }
                    Message::Close(_) => break,
                    _ => break,
                }
            }
        }
    }
    mark_offline(&registry, &certificate_edge_id, connection_id, &mut stop).await;
}

async fn wait_for_stop(stop: &mut watch::Receiver<bool>) {
    while !*stop.borrow() && stop.changed().await.is_ok() {}
}

async fn registry_write_until_stop<'a>(
    registry: &'a EdgeRegistry,
    stop: &mut watch::Receiver<bool>,
) -> Option<RwLockWriteGuard<'a, HashMap<String, EdgeEntry>>> {
    tokio::select! {
        biased;
        _ = wait_for_stop(stop) => None,
        entries = registry.write() => Some(entries),
    }
}

async fn send_control_message<S>(socket: &mut WebSocketStream<S>, message: Message) -> bool
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    matches!(timeout(CONTROL_IO_TIMEOUT, socket.send(message)).await, Ok(Ok(())))
}

async fn close_control_socket<S>(socket: &mut WebSocketStream<S>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let _ = timeout(CONTROL_IO_TIMEOUT, socket.close(None)).await;
}

fn registration_targets(targets: Vec<RegisteredTarget>) -> Option<BTreeMap<String, RegisteredTarget>> {
    let mut registered = BTreeMap::new();
    for target in targets {
        if target.target_id.is_empty() || target.display_name.is_empty() || registered.contains_key(&target.target_id) {
            return None;
        }
        registered.insert(target.target_id.clone(), target);
    }
    Some(registered)
}

async fn mark_offline(registry: &EdgeRegistry, edge_id: &str, connection_id: Uuid, stop: &mut watch::Receiver<bool>) {
    let Some(mut entries) = registry_write_until_stop(registry, stop).await else {
        return;
    };
    if let Some(entry) = entries.get_mut(edge_id).filter(|entry| entry.connection_id == connection_id) {
        entry.online = false;
    }
}

fn unix_time_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or_default()
}

fn internal_error(message: &str) -> GatewayError {
    GatewayError { code: GatewayErrorCode::Internal, message: message.to_string() }
}
