use std::collections::{BTreeMap, HashMap, HashSet};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::header::{CONNECTION, SEC_WEBSOCKET_ACCEPT, SEC_WEBSOCKET_KEY, SEC_WEBSOCKET_VERSION, UPGRADE};
use hyper::service::service_fn;
use hyper::upgrade::Upgraded;
use hyper::{Method, Request, Response, StatusCode, Version};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::{
    mpsc, oneshot, watch, Mutex as AsyncMutex, OwnedSemaphorePermit, RwLock, RwLockWriteGuard, Semaphore,
};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{sleep, timeout, Instant};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use uuid::Uuid;

use crate::config::{MainConfig, MainEnrollmentConfig, PkiEndpointConfig};
use crate::edge_gateway::{control_websocket_config, data_websocket_config};
use crate::health::HealthServer;
use crate::limits::{BufferBudget, ConnectionRateLimiter, IdentityConcurrency};
use crate::pki::{
    enroll_over_remote, enroll_over_unix, renew_over_remote, renew_over_unix, EnrollCsrRequest, RenewCsrRequest,
};
use crate::protocol::{
    decode_client_message, decode_edge_data_open, decode_edge_message, decode_edge_registration, encode_control_frame,
    ClientEvent, ClientMessage, EdgeToMain, GatewayEdgeRoutes, GatewayRoute, MainToEdge, RegisteredTarget, SessionId,
    SessionTickets, Stage, MAX_DATA_FRAME_SIZE,
};
use crate::reverse_proxy::{empty_proxy_body, full_proxy_body, FixedUpstreamProxy, ProxyBody};
use crate::state::GatewayState;
use crate::stream::relay_websockets;
use crate::tls::{GatewayTls, PeerIdentity};
use crate::{GatewayError, GatewayErrorCode};

const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(45);
const CONTROL_IO_TIMEOUT: Duration = Duration::from_secs(1);

pub type EdgeRegistry = Arc<RwLock<HashMap<String, EdgeEntry>>>;
type ServerSocket = WebSocketStream<TokioIo<Upgraded>>;
type PendingRoutes = Arc<AsyncMutex<HashMap<SessionId, PendingRoute>>>;

struct PendingRoute {
    edge_id: String,
    target_id: String,
    client_id: String,
    connection_id: Uuid,
    edge_stop: watch::Receiver<bool>,
    sender: oneshot::Sender<Result<EdgeDataChannel, GatewayErrorCode>>,
}

struct EdgeDataChannel {
    socket: ServerSocket,
    _permit: OwnedSemaphorePermit,
    connection_id: Uuid,
    edge_stop: watch::Receiver<bool>,
}

#[derive(Clone, Copy)]
enum EdgeSocketRole {
    Control,
    Data,
}

struct RouteConfig {
    edge_path: Arc<str>,
    edge_data_path: Arc<str>,
    dbx_path: Arc<str>,
    enrollment: Option<Arc<MainEnrollmentConfig>>,
    allowed_edge_ids: HashSet<String>,
    revoked_edge_serials: HashSet<String>,
    client_route_acl: BTreeMap<String, Vec<String>>,
    fallback: Option<Arc<FixedUpstreamProxy>>,
}

struct MainRuntime {
    tls: Arc<GatewayTls>,
    routes: Arc<RouteConfig>,
}

#[derive(Clone)]
struct MainImmutableConfig {
    listen: String,
    max_connections: usize,
    tls_handshake_timeout_secs: u64,
    http_header_timeout_secs: u64,
    health_listen: Option<String>,
    max_streams_per_edge: usize,
    max_streams_per_client: usize,
    connection_rate_per_second: u32,
    connection_rate_burst: u32,
    global_buffer_budget_bytes: usize,
    state_file: Option<std::path::PathBuf>,
}

struct StreamLimits {
    clients: IdentityConcurrency,
    edges: IdentityConcurrency,
    buffers: BufferBudget,
}

#[derive(Clone)]
struct MainServices {
    registry: EdgeRegistry,
    tickets: Arc<AsyncMutex<SessionTickets>>,
    pending_routes: PendingRoutes,
    control_tasks: Arc<Mutex<Vec<JoinHandle<()>>>>,
    stream_limits: Arc<StreamLimits>,
    state: Option<GatewayState>,
}

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
    pub session_shutdown: watch::Sender<bool>,
    pub online: bool,
}

pub struct MainGateway {
    local_addr: SocketAddr,
    registry: EdgeRegistry,
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
    runtime: Arc<ArcSwap<MainRuntime>>,
    immutable: MainImmutableConfig,
    health: Option<HealthServer>,
}

impl MainGateway {
    pub async fn bind(config: MainConfig) -> Result<Self, GatewayError> {
        let runtime = Arc::new(ArcSwap::from_pointee(build_runtime(&config)?));
        let immutable = MainImmutableConfig {
            listen: config.listen.clone(),
            max_connections: config.max_connections,
            tls_handshake_timeout_secs: config.tls_handshake_timeout_secs,
            http_header_timeout_secs: config.http_header_timeout_secs,
            health_listen: config.health_listen.clone(),
            max_streams_per_edge: config.max_streams_per_edge,
            max_streams_per_client: config.max_streams_per_client,
            connection_rate_per_second: config.connection_rate_per_second,
            connection_rate_burst: config.connection_rate_burst,
            global_buffer_budget_bytes: config.global_buffer_budget_bytes,
            state_file: config.state_file.clone(),
        };
        let listener =
            TcpListener::bind(&config.listen).await.map_err(|_| internal_error("main listener could not bind"))?;
        let local_addr = listener.local_addr().map_err(|_| internal_error("main listener address unavailable"))?;
        let state = match &config.state_file {
            Some(path) => Some(GatewayState::open(path.clone()).await?),
            None => None,
        };
        let registry = EdgeRegistry::default();
        if let Some(state) = &state {
            let mut entries = registry.write().await;
            for (edge_id, targets) in state.load_edge_routes().await? {
                let (control_tx, _) = mpsc::channel(1);
                let (session_shutdown, _) = watch::channel(false);
                entries.insert(
                    edge_id.clone(),
                    EdgeEntry {
                        identity: EdgeIdentity { edge_id, serial: String::new(), fingerprint_sha256: [0; 32] },
                        targets,
                        last_heartbeat: Instant::now(),
                        control_tx,
                        active_streams: 0,
                        connection_id: Uuid::new_v4(),
                        session_shutdown,
                        online: false,
                    },
                );
            }
        }
        let health = match &config.health_listen {
            Some(listen) => Some(
                HealthServer::bind(listen, registry.clone(), &config.certificate, config.enrollment.is_some()).await?,
            ),
            None => None,
        };
        let tickets = Arc::new(AsyncMutex::new(SessionTickets::new(Duration::from_secs(15))));
        let pending_routes = PendingRoutes::default();
        let control_tasks = Arc::new(Mutex::new(Vec::<JoinHandle<()>>::new()));
        let task_runtime = runtime.clone();
        let connection_slots = Arc::new(Semaphore::new(config.max_connections));
        let connection_rates =
            Arc::new(ConnectionRateLimiter::new(config.connection_rate_per_second, config.connection_rate_burst));
        let stream_limits = Arc::new(StreamLimits {
            clients: IdentityConcurrency::new(config.max_streams_per_client),
            edges: IdentityConcurrency::new(config.max_streams_per_edge),
            buffers: BufferBudget::new(config.global_buffer_budget_bytes),
        });
        let services =
            MainServices { registry: registry.clone(), tickets, pending_routes, control_tasks, stream_limits, state };
        let task_services = services.clone();
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
                let (stream, peer) = match accepted {
                    Ok((stream, peer)) => (stream, peer),
                    Err(_) => {
                        sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                if !connection_rates.allow(&peer.ip().to_string(), std::time::Instant::now()) {
                    continue;
                }
                let Ok(permit) = connection_slots.clone().try_acquire_owned() else {
                    continue;
                };
                let runtime = task_runtime.load_full();
                let acceptor = TlsAcceptor::from(runtime.tls.server_config.clone());
                let tls = runtime.tls.clone();
                let routes = runtime.routes.clone();
                let connection_stop = task_shutdown.subscribe();
                let services = task_services.clone();
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
                        request.extensions_mut().insert(identity.clone());
                        let routes = routes.clone();
                        let services = services.clone();
                        let connection_permit = connection_permit.clone();
                        let connection_stop = connection_stop.clone();
                        let first_request_tx = first_request_tx.clone();
                        async move {
                            let response =
                                route(request, routes, peer, services, connection_permit, connection_stop).await;
                            if let Ok(mut sender) = first_request_tx.lock() {
                                if let Some(sender) = sender.take() {
                                    let _ = sender.send(());
                                }
                            }
                            response
                        }
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
            let controls =
                task_services.control_tasks.lock().map(|mut tasks| std::mem::take(&mut *tasks)).unwrap_or_default();
            for control in controls {
                let _ = control.await;
            }
        });
        Ok(Self { local_addr, registry, shutdown, task, runtime, immutable, health })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn registry(&self) -> EdgeRegistry {
        self.registry.clone()
    }

    pub async fn reload(&self, config: MainConfig) -> Result<(), GatewayError> {
        if config.listen != self.immutable.listen
            || config.max_connections != self.immutable.max_connections
            || config.tls_handshake_timeout_secs != self.immutable.tls_handshake_timeout_secs
            || config.http_header_timeout_secs != self.immutable.http_header_timeout_secs
            || config.health_listen != self.immutable.health_listen
            || config.max_streams_per_edge != self.immutable.max_streams_per_edge
            || config.max_streams_per_client != self.immutable.max_streams_per_client
            || config.connection_rate_per_second != self.immutable.connection_rate_per_second
            || config.connection_rate_burst != self.immutable.connection_rate_burst
            || config.global_buffer_budget_bytes != self.immutable.global_buffer_budget_bytes
            || config.state_file != self.immutable.state_file
        {
            return Err(GatewayError {
                code: GatewayErrorCode::ConfigInvalid,
                message: "restart_required: immutable Main settings changed".to_string(),
            });
        }
        let runtime = Arc::new(build_runtime(&config)?);
        self.runtime.store(runtime.clone());
        let entries = self.registry.read().await;
        for entry in entries.values() {
            if !edge_allowed(&runtime.routes, &entry.identity.edge_id, &entry.identity.serial) {
                let _ = entry.session_shutdown.send(true);
            }
        }
        drop(entries);
        Ok(())
    }

    pub async fn shutdown(mut self) {
        let _ = self.shutdown.send(true);
        let _ = (&mut self.task).await;
        if let Some(health) = self.health.take() {
            health.shutdown().await;
        }
    }

    pub fn health_addr(&self) -> Option<SocketAddr> {
        self.health.as_ref().map(HealthServer::local_addr)
    }
}

fn build_runtime(config: &MainConfig) -> Result<MainRuntime, GatewayError> {
    let edge_path = Arc::<str>::from(config.edge_path.clone());
    Ok(MainRuntime {
        tls: Arc::new(GatewayTls::load(config)?),
        routes: Arc::new(RouteConfig {
            edge_data_path: Arc::<str>::from(format!("{}/data", edge_path.trim_end_matches('/'))),
            edge_path,
            dbx_path: Arc::<str>::from(config.dbx_path.clone()),
            enrollment: config.enrollment.clone().map(Arc::new),
            allowed_edge_ids: config.allowed_edge_ids.iter().cloned().collect(),
            revoked_edge_serials: config.revoked_edge_serials.iter().map(|serial| normalize_serial(serial)).collect(),
            client_route_acl: config.client_route_acl.clone(),
            fallback: config.fallback_upstream.as_deref().map(FixedUpstreamProxy::new).transpose()?.map(Arc::new),
        }),
    })
}

fn edge_allowed(routes: &RouteConfig, edge_id: &str, serial: &str) -> bool {
    (routes.allowed_edge_ids.is_empty() || routes.allowed_edge_ids.contains(edge_id))
        && !routes.revoked_edge_serials.contains(&normalize_serial(serial))
}

fn normalize_serial(serial: &str) -> String {
    serial.chars().filter(|character| character.is_ascii_hexdigit()).flat_map(char::to_lowercase).collect()
}

impl Drop for MainGateway {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
    }
}

async fn route(
    mut request: Request<Incoming>,
    routes: Arc<RouteConfig>,
    peer: SocketAddr,
    services: MainServices,
    connection_permit: Arc<Mutex<Option<OwnedSemaphorePermit>>>,
    stop: watch::Receiver<bool>,
) -> Result<Response<ProxyBody>, Infallible> {
    let identity = request.extensions().get::<PeerIdentity>().cloned();
    if matches!(identity, Some(PeerIdentity::Anonymous))
        && routes.enrollment.as_ref().is_some_and(|config| request.uri().path() == config.path)
    {
        return Ok(handle_enrollment(request, routes.enrollment.clone().expect("checked above")).await);
    }
    if let Some(PeerIdentity::Edge { edge_id, serial, fingerprint_sha256 }) = identity {
        if !edge_allowed(&routes, &edge_id, &serial) {
            return Ok(empty_response(StatusCode::NOT_FOUND));
        }
        if routes.enrollment.as_ref().is_some_and(|config| request.uri().path() == config.renewal_path) {
            return Ok(
                handle_renewal(request, routes.enrollment.clone().expect("checked above"), edge_id, serial).await
            );
        }
        if request.uri().path() == routes.edge_path.as_ref() || request.uri().path() == routes.edge_data_path.as_ref() {
            if let Some(accept_key) = websocket_accept_key(&request) {
                let Some(permit) = connection_permit.lock().ok().and_then(|mut permit| permit.take()) else {
                    return Ok(empty_response(StatusCode::NOT_FOUND));
                };
                let version = request.version();
                let (websocket_config, socket_role) = if request.uri().path() == routes.edge_data_path.as_ref() {
                    (data_websocket_config(), EdgeSocketRole::Data)
                } else {
                    (control_websocket_config(), EdgeSocketRole::Control)
                };
                let task_services = services.clone();
                let control = tokio::spawn(async move {
                    let Ok(upgraded) = hyper::upgrade::on(&mut request).await else {
                        return;
                    };
                    let socket =
                        WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, Some(websocket_config))
                            .await;
                    run_edge_control(
                        socket,
                        permit,
                        socket_role,
                        edge_id,
                        serial,
                        fingerprint_sha256,
                        task_services,
                        stop,
                    )
                    .await;
                });
                track_task(&services.control_tasks, control);
                return Ok(switching_protocols(version, accept_key));
            }
            return Ok(empty_response(StatusCode::OK));
        }
        return Ok(empty_response(StatusCode::NOT_FOUND));
    }
    if let Some(PeerIdentity::DbxClient { client_id, .. }) = identity {
        if request.uri().path() == routes.dbx_path.as_ref() {
            if let Some(accept_key) = websocket_accept_key(&request) {
                let Some(permit) = connection_permit.lock().ok().and_then(|mut permit| permit.take()) else {
                    return Ok(empty_response(StatusCode::NOT_FOUND));
                };
                let version = request.version();
                let task_services = services.clone();
                let task = tokio::spawn(async move {
                    let Ok(upgraded) = hyper::upgrade::on(&mut request).await else { return };
                    let socket = WebSocketStream::from_raw_socket(
                        TokioIo::new(upgraded),
                        Role::Server,
                        Some(data_websocket_config()),
                    )
                    .await;
                    run_dbx_connection(socket, permit, client_id, routes, task_services, stop).await;
                });
                track_task(&services.control_tasks, task);
                return Ok(switching_protocols(version, accept_key));
            }
            return Ok(empty_response(StatusCode::OK));
        }
    }
    if reserved_path(&routes, request.uri().path()) {
        return Ok(empty_response(StatusCode::NOT_FOUND));
    }
    if let Some(proxy) = &routes.fallback {
        return Ok(proxy.fallback(request, peer).await.unwrap_or_else(|_| empty_response(StatusCode::BAD_GATEWAY)));
    }
    Ok(empty_response(StatusCode::NOT_FOUND))
}

fn reserved_path(routes: &RouteConfig, path: &str) -> bool {
    path == routes.edge_path.as_ref()
        || path == routes.edge_data_path.as_ref()
        || path == routes.dbx_path.as_ref()
        || routes
            .enrollment
            .as_ref()
            .is_some_and(|enrollment| path == enrollment.path || path == enrollment.renewal_path)
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct EdgeRenewalBody {
    csr_der: Vec<u8>,
}

async fn handle_renewal(
    request: Request<Incoming>,
    config: Arc<MainEnrollmentConfig>,
    edge_id: String,
    current_serial: String,
) -> Response<ProxyBody> {
    if request.method() != Method::POST {
        return empty_response(StatusCode::NOT_FOUND);
    }
    let body = match Limited::new(request.into_body(), 256 * 1024).collect().await {
        Ok(body) => body.to_bytes(),
        Err(_) => return empty_response(StatusCode::PAYLOAD_TOO_LARGE),
    };
    let body: EdgeRenewalBody = match serde_json::from_slice(&body) {
        Ok(body) => body,
        Err(_) => return empty_response(StatusCode::BAD_REQUEST),
    };
    let renewal = RenewCsrRequest { edge_id, current_serial, csr_der: body.csr_der };
    let response = match &config.pki {
        PkiEndpointConfig::Unix { unix_socket } => renew_over_unix(unix_socket, &renewal).await,
        PkiEndpointConfig::Remote { remote_address, server_name, ca_certificate, certificate, private_key } => {
            match remote_address.parse() {
                Ok(address) => {
                    renew_over_remote(address, server_name, ca_certificate, certificate, private_key, &renewal).await
                }
                Err(_) => return empty_response(StatusCode::NOT_FOUND),
            }
        }
    };
    json_response(response)
}

fn json_response(response: Result<crate::pki::EnrollCsrResponse, GatewayError>) -> Response<ProxyBody> {
    let Ok(response) = response else {
        return empty_response(StatusCode::NOT_FOUND);
    };
    match serde_json::to_vec(&response) {
        Ok(body) => Response::builder()
            .status(StatusCode::OK)
            .header(hyper::header::CONTENT_TYPE, "application/json")
            .body(full_proxy_body(Bytes::from(body)))
            .expect("fixed response is valid"),
        Err(_) => empty_response(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

fn track_task(tasks: &Arc<Mutex<Vec<JoinHandle<()>>>>, task: JoinHandle<()>) {
    if let Ok(mut tasks) = tasks.lock() {
        tasks.retain(|task| !task.is_finished());
        tasks.push(task);
    }
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

fn switching_protocols(version: Version, accept_key: String) -> Response<ProxyBody> {
    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .version(version)
        .header(CONNECTION, "Upgrade")
        .header(UPGRADE, "websocket")
        .header(SEC_WEBSOCKET_ACCEPT, accept_key)
        .body(empty_proxy_body())
        .expect("fixed WebSocket response is valid")
}

fn empty_response(status: StatusCode) -> Response<ProxyBody> {
    Response::builder().status(status).body(empty_proxy_body()).expect("fixed response is valid")
}

async fn handle_enrollment(request: Request<Incoming>, config: Arc<MainEnrollmentConfig>) -> Response<ProxyBody> {
    if request.method() != Method::POST {
        return empty_response(StatusCode::NOT_FOUND);
    }
    let body = match Limited::new(request.into_body(), 256 * 1024).collect().await {
        Ok(body) => body.to_bytes(),
        Err(_) => return empty_response(StatusCode::PAYLOAD_TOO_LARGE),
    };
    let request: EnrollCsrRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(_) => return empty_response(StatusCode::BAD_REQUEST),
    };
    if !config.allowed_edge_ids.iter().any(|edge_id| edge_id == &request.claimed_edge_id) {
        return empty_response(StatusCode::NOT_FOUND);
    }
    let response = match &config.pki {
        PkiEndpointConfig::Unix { unix_socket } => enroll_over_unix(unix_socket, &request).await,
        PkiEndpointConfig::Remote { remote_address, server_name, ca_certificate, certificate, private_key } => {
            match remote_address.parse() {
                Ok(address) => {
                    enroll_over_remote(address, server_name, ca_certificate, certificate, private_key, &request).await
                }
                Err(_) => return empty_response(StatusCode::NOT_FOUND),
            }
        }
    };
    json_response(response)
}

async fn run_edge_control(
    mut socket: ServerSocket,
    permit: OwnedSemaphorePermit,
    socket_role: EdgeSocketRole,
    certificate_edge_id: String,
    serial: String,
    fingerprint_sha256: [u8; 32],
    services: MainServices,
    mut stop: watch::Receiver<bool>,
) {
    let MainServices { registry, tickets, pending_routes, state, .. } = services;
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
    if matches!(socket_role, EdgeSocketRole::Data) {
        let Ok(open) = decode_edge_data_open(&frame) else {
            close_control_socket(&mut socket).await;
            return;
        };
        accept_edge_data_channel(
            socket,
            permit,
            &certificate_edge_id,
            open.session_id,
            &open.target_id,
            registry,
            tickets,
            pending_routes,
        )
        .await;
        return;
    }
    let _permit = permit;
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
    let (session_shutdown, mut session_stop) = watch::channel(false);
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
                    session_shutdown: session_shutdown.clone(),
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
    if let Some(state) = &state {
        let targets = registry.read().await[&certificate_edge_id].targets.clone();
        if state.replace_edge_routes(&certificate_edge_id, &targets).await.is_err() {
            let mut entries = registry.write().await;
            if entries.get(&certificate_edge_id).is_some_and(|entry| entry.connection_id == connection_id) {
                entries.remove(&certificate_edge_id);
            }
            close_control_socket(&mut socket).await;
            return;
        }
    }

    loop {
        tokio::select! {
            changed = session_stop.changed() => {
                if changed.is_err() || *session_stop.borrow() {
                    close_control_socket(&mut socket).await;
                    break;
                }
            }
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
                    Message::Binary(frame) => match decode_edge_message(&frame) {
                        Ok(EdgeToMain::Heartbeat { .. }) => {
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
                        Ok(EdgeToMain::DataChannelFailed { session_id, .. }) => {
                            let route = {
                                let mut routes = pending_routes.lock().await;
                                let matches = routes
                                    .get(&session_id)
                                    .is_some_and(|route| {
                                        route.edge_id == certificate_edge_id && route.connection_id == connection_id
                                    });
                                matches.then(|| routes.remove(&session_id)).flatten()
                            };
                            if let Some(route) = route {
                                let _ = route.sender.send(Err(GatewayErrorCode::TargetUnavailable));
                            }
                        }
                        Err(_) => break,
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
    let _ = session_shutdown.send(true);
    mark_offline(&registry, &certificate_edge_id, connection_id, &mut stop).await;
}

async fn accept_edge_data_channel(
    mut socket: ServerSocket,
    permit: OwnedSemaphorePermit,
    edge_id: &str,
    session_id: SessionId,
    target_id: &str,
    registry: EdgeRegistry,
    tickets: Arc<AsyncMutex<SessionTickets>>,
    pending_routes: PendingRoutes,
) {
    let route = {
        let mut routes = pending_routes.lock().await;
        let matches =
            routes.get(&session_id).is_some_and(|route| route.edge_id == edge_id && route.target_id == target_id);
        matches.then(|| routes.remove(&session_id)).flatten()
    };
    let Some(route) = route else {
        close_control_socket(&mut socket).await;
        return;
    };
    let still_online = registry
        .read()
        .await
        .get(edge_id)
        .is_some_and(|entry| entry.online && entry.connection_id == route.connection_id);
    if !still_online {
        close_control_socket(&mut socket).await;
        return;
    }
    if tickets.lock().await.consume(&session_id, edge_id, target_id, &route.client_id).is_err() {
        close_control_socket(&mut socket).await;
        return;
    }
    if let Err(Ok(channel)) = route.sender.send(Ok(EdgeDataChannel {
        socket,
        _permit: permit,
        connection_id: route.connection_id,
        edge_stop: route.edge_stop,
    })) {
        let mut socket = channel.socket;
        close_control_socket(&mut socket).await;
    }
}

async fn run_dbx_connection(
    mut socket: ServerSocket,
    _permit: OwnedSemaphorePermit,
    client_id: String,
    routes: Arc<RouteConfig>,
    services: MainServices,
    mut stop: watch::Receiver<bool>,
) {
    let MainServices { registry, tickets, pending_routes, stream_limits: limits, .. } = services;
    if !send_client_event(&mut socket, ClientEvent::Stage { stage: Stage::MainAuthenticated }).await {
        return;
    }
    let Some(_client_stream_permit) = limits.clients.try_acquire(&client_id) else {
        send_client_error(&mut socket, GatewayErrorCode::CapacityExceeded).await;
        return;
    };
    let incoming = tokio::select! {
        incoming = timeout(Duration::from_secs(10), socket.next()) => incoming,
        _ = wait_for_stop(&mut stop) => return,
    };
    let Ok(Some(Ok(Message::Binary(frame)))) = incoming else {
        close_control_socket(&mut socket).await;
        return;
    };
    let Ok(message) = decode_client_message(&frame) else {
        send_client_error(&mut socket, GatewayErrorCode::ProtocolMismatch).await;
        return;
    };
    let ClientMessage::OpenRoute { edge_id, target_id, .. } = message else {
        let entries = registry.read().await;
        let edges = entries
            .iter()
            .map(|(edge_id, entry)| GatewayEdgeRoutes {
                edge_id: edge_id.clone(),
                online: entry.online,
                routes: entry
                    .targets
                    .values()
                    .filter(|target| {
                        client_route_allowed(&routes.client_route_acl, &client_id, edge_id, &target.target_id)
                    })
                    .map(|target| GatewayRoute {
                        target_id: target.target_id.clone(),
                        display_name: target.display_name.clone(),
                    })
                    .collect(),
            })
            .collect();
        let _ = send_client_event(&mut socket, ClientEvent::Routes { edges }).await;
        close_control_socket(&mut socket).await;
        return;
    };
    if !client_route_allowed(&routes.client_route_acl, &client_id, &edge_id, &target_id) {
        send_client_error(&mut socket, GatewayErrorCode::RouteDenied).await;
        return;
    }
    let route_control = {
        let entries = registry.read().await;
        match entries.get(&edge_id) {
            Some(entry) if !entry.online => Err(GatewayErrorCode::EdgeOffline),
            Some(entry) if !entry.targets.contains_key(&target_id) => Err(GatewayErrorCode::RouteDenied),
            Some(entry) => Ok((entry.control_tx.clone(), entry.connection_id, entry.session_shutdown.subscribe())),
            None => Err(GatewayErrorCode::EdgeOffline),
        }
    };
    let (control_tx, connection_id, mut edge_stop) = match route_control {
        Ok(route_control) => route_control,
        Err(code) => {
            send_client_error(&mut socket, code).await;
            return;
        }
    };
    let Some(_edge_stream_permit) = limits.edges.try_acquire(&edge_id) else {
        send_client_error(&mut socket, GatewayErrorCode::CapacityExceeded).await;
        return;
    };
    let Some(_buffer_reservation) = limits.buffers.try_reserve(MAX_DATA_FRAME_SIZE * 2) else {
        send_client_error(&mut socket, GatewayErrorCode::CapacityExceeded).await;
        return;
    };
    if !send_client_event(&mut socket, ClientEvent::Stage { stage: Stage::RouteAuthorized }).await {
        return;
    }
    let ticket = match tickets.lock().await.issue(&edge_id, &target_id, &client_id) {
        Ok(ticket) => ticket,
        Err(error) => {
            send_client_error(&mut socket, error.code).await;
            return;
        }
    };
    let (sender, receiver) = oneshot::channel();
    pending_routes.lock().await.insert(
        ticket.session_id,
        PendingRoute {
            edge_id: edge_id.clone(),
            target_id: target_id.clone(),
            client_id,
            connection_id,
            edge_stop: edge_stop.clone(),
            sender,
        },
    );
    let request = MainToEdge::OpenDataChannel {
        session_id: ticket.session_id,
        target_id: target_id.clone(),
        expires_at_unix_ms: ticket.expires_at_unix_ms,
    };
    if !matches!(timeout(CONTROL_IO_TIMEOUT, control_tx.send(request)).await, Ok(Ok(()))) {
        cleanup_pending(ticket.session_id, &pending_routes, &tickets).await;
        send_client_error(&mut socket, GatewayErrorCode::EdgeOffline).await;
        return;
    }
    let channel = tokio::select! {
        channel = timeout(Duration::from_secs(15), receiver) => channel,
        _ = wait_for_stop(&mut stop) => {
            cleanup_pending(ticket.session_id, &pending_routes, &tickets).await;
            return;
        },
        _ = wait_for_stop(&mut edge_stop) => {
            cleanup_pending(ticket.session_id, &pending_routes, &tickets).await;
            send_client_error(&mut socket, GatewayErrorCode::EdgeOffline).await;
            return;
        },
        _ = socket.next() => {
            cleanup_pending(ticket.session_id, &pending_routes, &tickets).await;
            return;
        },
    };
    let Ok(Ok(Ok(channel))) = channel else {
        let code = match channel {
            Ok(Ok(Err(code))) => code,
            _ => GatewayErrorCode::TargetUnavailable,
        };
        cleanup_pending(ticket.session_id, &pending_routes, &tickets).await;
        send_client_error(&mut socket, code).await;
        return;
    };
    let EdgeDataChannel { socket: edge_socket, _permit: edge_permit, connection_id, edge_stop } = channel;
    let _edge_permit = edge_permit;
    if !adjust_active_streams(&registry, &edge_id, connection_id, true, &mut stop).await {
        return;
    }
    for stage in [Stage::EdgeChannelReady, Stage::TargetConnected, Stage::StreamReady] {
        if !send_client_event(&mut socket, ClientEvent::Stage { stage }).await {
            let _ = adjust_active_streams(&registry, &edge_id, connection_id, false, &mut stop).await;
            return;
        }
    }
    let (stream_stop, stop_task) = combine_stop(stop.clone(), edge_stop);
    let _ = relay_websockets(socket, edge_socket, Duration::from_secs(300), stream_stop).await;
    stop_task.abort();
    let _ = adjust_active_streams(&registry, &edge_id, connection_id, false, &mut stop).await;
}

fn client_route_allowed(acl: &BTreeMap<String, Vec<String>>, client_id: &str, edge_id: &str, target_id: &str) -> bool {
    if acl.is_empty() {
        return true;
    }
    acl.get(client_id).is_some_and(|rules| {
        rules.iter().any(|rule| {
            let Some((allowed_edge, allowed_target)) = rule.split_once('/') else { return false };
            (allowed_edge == "*" || allowed_edge == edge_id) && (allowed_target == "*" || allowed_target == target_id)
        })
    })
}

async fn adjust_active_streams(
    registry: &EdgeRegistry,
    edge_id: &str,
    connection_id: Uuid,
    increment: bool,
    stop: &mut watch::Receiver<bool>,
) -> bool {
    let Some(mut entries) = registry_write_until_stop(registry, stop).await else { return false };
    let Some(entry) = entries.get_mut(edge_id).filter(|entry| entry.connection_id == connection_id) else {
        return false;
    };
    entry.active_streams =
        if increment { entry.active_streams.saturating_add(1) } else { entry.active_streams.saturating_sub(1) };
    true
}

async fn cleanup_pending(
    session_id: SessionId,
    pending_routes: &PendingRoutes,
    tickets: &Arc<AsyncMutex<SessionTickets>>,
) {
    pending_routes.lock().await.remove(&session_id);
    tickets.lock().await.discard(&session_id);
}

fn combine_stop(
    mut global: watch::Receiver<bool>,
    mut edge: watch::Receiver<bool>,
) -> (watch::Receiver<bool>, JoinHandle<()>) {
    let (sender, receiver) = watch::channel(false);
    let task = tokio::spawn(async move {
        tokio::select! {
            _ = wait_for_stop(&mut global) => {}
            _ = wait_for_stop(&mut edge) => {}
        }
        let _ = sender.send(true);
    });
    (receiver, task)
}

async fn send_client_event(socket: &mut ServerSocket, event: ClientEvent) -> bool {
    let Ok(frame) = encode_control_frame(&event) else { return false };
    send_control_message(socket, Message::Binary(frame.into())).await
}

async fn send_client_error(socket: &mut ServerSocket, code: GatewayErrorCode) {
    let _ = send_client_event(socket, ClientEvent::Error { code }).await;
    close_control_socket(socket).await;
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
