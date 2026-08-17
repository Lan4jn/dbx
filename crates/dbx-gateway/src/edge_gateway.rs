use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use rcgen::{CertificateParams, DnType, KeyPair};
use rustls::ClientConfig;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{interval_at, sleep_until, Instant, MissedTickBehavior};
use tokio_rustls::TlsConnector;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async_tls_with_config, Connector, MaybeTlsStream, WebSocketStream};
use x509_parser::extensions::GeneralName;
use x509_parser::pem::parse_x509_pem;
use x509_parser::prelude::parse_x509_certificate;
use zeroize::Zeroizing;

use crate::config::{EdgeBootstrapConfig, EdgeConfig, TargetAddress};
use crate::limits::TargetPolicy;
use crate::pki::{EnrollCsrRequest, EnrollCsrResponse};
use crate::protocol::{
    decode_control_frame, encode_control_frame, EdgeDataOpen, EdgeRegistration, EdgeToMain, MainToEdge,
    ProtocolVersion, RegisteredTarget, SessionId, MAX_CONTROL_FRAME_SIZE, MAX_DATA_FRAME_SIZE,
};
use crate::stream::relay_websocket_to_io;
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
        let tls = match credential_state(&config) {
            EdgeCredentialState::Enrolled { .. } => Some(edge_tls(&config)?),
            EdgeCredentialState::Bootstrap { .. } => None,
            EdgeCredentialState::Unavailable { .. } => return Err(internal_error("Edge credentials are unavailable")),
        };
        let (shutdown, stop) = watch::channel(false);
        let task = tokio::spawn(run_entry(config, tls, stop));
        Ok(Self { shutdown, task })
    }

    pub async fn shutdown(mut self) {
        let _ = self.shutdown.send(true);
        let _ = (&mut self.task).await;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EdgeCredentialState {
    Enrolled { certificate: PathBuf, private_key: PathBuf },
    Bootstrap { token_file: PathBuf },
    Unavailable { reason: String },
}

pub fn credential_state(config: &EdgeConfig) -> EdgeCredentialState {
    if config.certificate.is_file() && config.private_key.is_file() {
        if config.bootstrap.as_ref().is_some_and(|bootstrap| bootstrap.token_file.exists()) {
            EdgeCredentialState::Unavailable { reason: "bootstrap token cleanup is incomplete".to_string() }
        } else {
            EdgeCredentialState::Enrolled {
                certificate: config.certificate.clone(),
                private_key: config.private_key.clone(),
            }
        }
    } else if let Some(bootstrap) = &config.bootstrap {
        EdgeCredentialState::Bootstrap { token_file: bootstrap.token_file.clone() }
    } else {
        EdgeCredentialState::Unavailable { reason: "Edge credentials are unavailable".to_string() }
    }
}

fn edge_tls(config: &EdgeConfig) -> Result<Arc<ClientConfig>, GatewayError> {
    Ok(Arc::new(
        ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
            .with_root_certificates(load_roots(&config.ca_certificate)?)
            .with_client_auth_cert(load_certificates(&config.certificate)?, load_private_key(&config.private_key)?)
            .map_err(|_| tls_error("Edge certificate or private key was rejected"))?,
    ))
}

async fn run_entry(config: EdgeConfig, tls: Option<Arc<ClientConfig>>, stop: watch::Receiver<bool>) {
    let mut tls = match tls {
        Some(tls) => tls,
        None => {
            if bootstrap_edge(&config).await.is_err() {
                return;
            }
            match edge_tls(&config) {
                Ok(tls) => tls,
                Err(_) => return,
            }
        }
    };
    if config
        .bootstrap
        .as_ref()
        .is_some_and(|bootstrap| certificate_needs_renewal(&config.certificate, bootstrap.renew_before_days))
        && renew_edge_certificate(&config, tls.clone()).await.is_ok()
    {
        if let Ok(reloaded) = edge_tls(&config) {
            tls = reloaded;
        }
    }
    run(config, tls, stop).await;
}

async fn renew_edge_certificate(config: &EdgeConfig, tls: Arc<ClientConfig>) -> Result<(), GatewayError> {
    let bootstrap = config.bootstrap.as_ref().ok_or_else(|| internal_error("Edge renewal endpoint is unavailable"))?;
    let key_pem = Zeroizing::new(
        fs::read_to_string(&config.private_key).map_err(|_| internal_error("Edge private key could not be read"))?,
    );
    let key = KeyPair::from_pem(&key_pem).map_err(|_| internal_error("Edge private key could not be parsed"))?;
    let csr = CertificateParams::default()
        .serialize_request(&key)
        .map_err(|_| internal_error("Edge renewal CSR could not be generated"))?;
    let renewal_url = renewal_url(&bootstrap.enrollment_url)?;
    let body = serde_json::to_vec(&serde_json::json!({ "csr_der": csr.der().as_ref() }))
        .map_err(|_| internal_error("Edge renewal request could not be encoded"))?;
    let response = post_main_request(&renewal_url, tls, &bootstrap.server_spki_sha256, &body).await?;
    let response: EnrollCsrResponse =
        serde_json::from_slice(&response).map_err(|_| internal_error("Edge renewal response was invalid"))?;
    validate_enrolled_certificate(&response, &config.edge_id, key.public_key_raw())?;
    let certificate_temp = temporary_path(&config.certificate);
    let chain = format!("{}{}", response.certificate_pem, response.chain_pem);
    write_public_temp(&certificate_temp, chain.as_bytes())?;
    fs::rename(&certificate_temp, &config.certificate)
        .map_err(|_| internal_error("renewed Edge certificate could not be installed"))?;
    sync_parent(&config.certificate)
}

fn certificate_needs_renewal(path: &std::path::Path, renew_before_days: u64) -> bool {
    let Ok(pem) = fs::read(path) else { return false };
    let Ok((_, pem)) = parse_x509_pem(&pem) else { return false };
    let Ok((_, certificate)) = parse_x509_certificate(&pem.contents) else {
        return false;
    };
    let Ok(days) = i64::try_from(renew_before_days) else {
        return false;
    };
    certificate.validity().not_after.to_datetime() <= time::OffsetDateTime::now_utc() + time::Duration::days(days)
}

fn renewal_url(enrollment_url: &str) -> Result<String, GatewayError> {
    let mut uri: hyper::Uri = enrollment_url.parse().map_err(|_| internal_error("Edge renewal URL is invalid"))?;
    let renewed_path = uri
        .path()
        .strip_suffix("/enroll")
        .map(|prefix| format!("{prefix}/renew"))
        .unwrap_or_else(|| format!("{}/renew", uri.path().trim_end_matches('/')));
    let mut parts = uri.into_parts();
    parts.path_and_query = Some(renewed_path.parse().map_err(|_| internal_error("Edge renewal URL is invalid"))?);
    uri = hyper::Uri::from_parts(parts).map_err(|_| internal_error("Edge renewal URL is invalid"))?;
    Ok(uri.to_string())
}

async fn bootstrap_edge(config: &EdgeConfig) -> Result<(), GatewayError> {
    let bootstrap =
        config.bootstrap.as_ref().ok_or_else(|| internal_error("Edge bootstrap configuration is unavailable"))?;
    let token = read_bootstrap_token(&bootstrap.token_file)?;
    let key = KeyPair::generate().map_err(|_| internal_error("Edge private key could not be generated"))?;
    let mut params = CertificateParams::default();
    params.distinguished_name.push(DnType::CommonName, &config.edge_id);
    let csr = params.serialize_request(&key).map_err(|_| internal_error("Edge CSR could not be generated"))?;
    let private_key_pem = Zeroizing::new(key.serialize_pem());
    let private_temp = temporary_path(&config.private_key);
    write_private_temp(&private_temp, private_key_pem.as_bytes())?;
    let result = async {
        let response = request_enrollment(
            bootstrap,
            &config.ca_certificate,
            &EnrollCsrRequest { token, claimed_edge_id: config.edge_id.clone(), csr_der: csr.der().to_vec() },
        )
        .await?;
        validate_enrolled_certificate(&response, &config.edge_id, key.public_key_raw())?;
        let certificate_temp = temporary_path(&config.certificate);
        let chain = format!("{}{}", response.certificate_pem, response.chain_pem);
        write_public_temp(&certificate_temp, chain.as_bytes())?;
        fs::rename(&private_temp, &config.private_key)
            .map_err(|_| internal_error("Edge private key could not be installed"))?;
        if fs::rename(&certificate_temp, &config.certificate).is_err() {
            let _ = fs::remove_file(&config.private_key);
            return Err(internal_error("Edge certificate could not be installed"));
        }
        sync_parent(&config.certificate)?;
        fs::remove_file(&bootstrap.token_file)
            .map_err(|_| internal_error("Edge bootstrap token could not be deleted"))?;
        sync_parent(&bootstrap.token_file)
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_file(&private_temp);
    }
    result
}

async fn request_enrollment(
    bootstrap: &EdgeBootstrapConfig,
    ca_certificate: &std::path::Path,
    request: &EnrollCsrRequest,
) -> Result<EnrollCsrResponse, GatewayError> {
    let client = ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .with_root_certificates(load_roots(ca_certificate)?)
        .with_no_client_auth();
    let body =
        serde_json::to_vec(request).map_err(|_| internal_error("Edge enrollment request could not be encoded"))?;
    let response =
        post_main_request(&bootstrap.enrollment_url, Arc::new(client), &bootstrap.server_spki_sha256, &body).await?;
    serde_json::from_slice(&response).map_err(|_| internal_error("Edge enrollment response was invalid"))
}

async fn post_main_request(
    url: &str,
    client: Arc<ClientConfig>,
    server_spki_sha256: &str,
    body: &[u8],
) -> Result<Vec<u8>, GatewayError> {
    let uri: hyper::Uri = url.parse().map_err(|_| internal_error("Main request URL is invalid"))?;
    if uri.scheme_str() != Some("https") {
        return Err(internal_error("Main request URL is invalid"));
    }
    let host = uri.host().ok_or_else(|| internal_error("Edge enrollment URL is invalid"))?;
    let port = uri.port_u16().unwrap_or(443);
    let stream = TcpStream::connect((host, port))
        .await
        .map_err(|_| internal_error("Main enrollment endpoint is unavailable"))?;
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|_| internal_error("Edge enrollment server name is invalid"))?;
    let mut stream = TlsConnector::from(client)
        .connect(server_name, stream)
        .await
        .map_err(|_| internal_error("Main enrollment TLS was rejected"))?;
    verify_server_pin(stream.get_ref().1.peer_certificates(), server_spki_sha256)?;
    let path = uri.path_and_query().map(|value| value.as_str()).unwrap_or("/");
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await.map_err(|_| internal_error("Edge enrollment request could not be sent"))?;
    stream.write_all(body).await.map_err(|_| internal_error("Edge enrollment request could not be sent"))?;
    let mut response = Vec::new();
    stream
        .take(512 * 1024)
        .read_to_end(&mut response)
        .await
        .map_err(|_| internal_error("Edge enrollment response could not be read"))?;
    let separator = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| internal_error("Edge enrollment response was invalid"))?;
    let head = std::str::from_utf8(&response[..separator])
        .map_err(|_| internal_error("Edge enrollment response was invalid"))?;
    if !head.lines().next().is_some_and(|line| line.contains(" 200 ")) {
        return Err(internal_error("Edge enrollment was rejected"));
    }
    Ok(response[separator + 4..].to_vec())
}

fn verify_server_pin(
    certificates: Option<&[rustls::pki_types::CertificateDer<'static>]>,
    expected: &str,
) -> Result<(), GatewayError> {
    let leaf = certificates
        .and_then(|certificates| certificates.first())
        .ok_or_else(|| internal_error("Main enrollment certificate is unavailable"))?;
    let (_, certificate) =
        parse_x509_certificate(leaf.as_ref()).map_err(|_| internal_error("Main enrollment certificate is invalid"))?;
    let actual = hex::encode(Sha256::digest(certificate.public_key().raw));
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(internal_error("Main enrollment SPKI pin was rejected"))
    }
}

fn validate_enrolled_certificate(
    response: &EnrollCsrResponse,
    edge_id: &str,
    public_key: &[u8],
) -> Result<(), GatewayError> {
    if response.edge_id != edge_id {
        return Err(internal_error("enrolled Edge identity did not match"));
    }
    let (_, pem) = parse_x509_pem(response.certificate_pem.as_bytes())
        .map_err(|_| internal_error("enrolled Edge certificate was invalid"))?;
    let (_, certificate) =
        parse_x509_certificate(&pem.contents).map_err(|_| internal_error("enrolled Edge certificate was invalid"))?;
    let sans = certificate
        .subject_alternative_name()
        .map_err(|_| internal_error("enrolled Edge certificate was invalid"))?
        .ok_or_else(|| internal_error("enrolled Edge certificate was invalid"))?;
    let expected = format!("urn:dbx-gateway:edge:{edge_id}");
    let matches = sans
        .value
        .general_names
        .iter()
        .filter_map(|name| match name {
            GeneralName::URI(uri) => Some(*uri),
            _ => None,
        })
        .eq([expected.as_str()]);
    if !matches || certificate.public_key().subject_public_key.data.as_ref() != public_key {
        return Err(internal_error("enrolled Edge certificate was invalid"));
    }
    Ok(())
}

fn read_bootstrap_token(path: &std::path::Path) -> Result<Zeroizing<String>, GatewayError> {
    let token = fs::read_to_string(path).map_err(|_| internal_error("Edge bootstrap token could not be read"))?;
    let token = token.trim().to_string();
    if token.is_empty() {
        Err(internal_error("Edge bootstrap token was empty"))
    } else {
        Ok(Zeroizing::new(token))
    }
}

fn temporary_path(path: &std::path::Path) -> PathBuf {
    path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()))
}

fn write_private_temp(path: &std::path::Path, bytes: &[u8]) -> Result<(), GatewayError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file =
        options.open(path).map_err(|_| internal_error("Edge private key temporary file could not be created"))?;
    file.write_all(bytes).map_err(|_| internal_error("Edge private key temporary file could not be written"))?;
    file.sync_all().map_err(|_| internal_error("Edge private key temporary file could not be synced"))
}

fn write_public_temp(path: &std::path::Path, bytes: &[u8]) -> Result<(), GatewayError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|_| internal_error("Edge certificate temporary file could not be created"))?;
    file.write_all(bytes).map_err(|_| internal_error("Edge certificate temporary file could not be written"))?;
    file.sync_all().map_err(|_| internal_error("Edge certificate temporary file could not be synced"))
}

fn sync_parent(path: &std::path::Path) -> Result<(), GatewayError> {
    let parent = path.parent().ok_or_else(|| internal_error("Edge credential directory is invalid"))?;
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| internal_error("Edge credential directory could not be synced"))
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
    let mut data_tasks = JoinSet::new();
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
            let (registered, disconnected_at) =
                run_control_session(socket, &config, tls.clone(), stop.clone(), &mut data_tasks).await;
            data_tasks.abort_all();
            while data_tasks.join_next().await.is_some() {}
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

async fn connect_data(
    config: &EdgeConfig,
    tls: Arc<ClientConfig>,
) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, GatewayError> {
    let url = format!("{}/data", config.main_url.trim_end_matches('/'));
    connect_async_tls_with_config(&url, Some(data_websocket_config()), false, Some(Connector::Rustls(tls)))
        .await
        .map(|(socket, _)| socket)
        .map_err(|_| internal_error("Edge data connection failed"))
}

pub(crate) fn control_websocket_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .read_buffer_size(16 * 1024)
        .write_buffer_size(16 * 1024)
        .max_write_buffer_size(MAX_CONTROL_FRAME_SIZE * 2)
        .max_message_size(Some(MAX_CONTROL_FRAME_SIZE))
        .max_frame_size(Some(MAX_CONTROL_FRAME_SIZE))
}

pub(crate) fn data_websocket_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .read_buffer_size(64 * 1024)
        .write_buffer_size(64 * 1024)
        .max_write_buffer_size(MAX_DATA_FRAME_SIZE * 2)
        .max_message_size(Some(MAX_DATA_FRAME_SIZE))
        .max_frame_size(Some(MAX_DATA_FRAME_SIZE))
}

async fn run_control_session(
    mut socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    config: &EdgeConfig,
    tls: Arc<ClientConfig>,
    mut stop: watch::Receiver<bool>,
    data_tasks: &mut JoinSet<Option<EdgeToMain>>,
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
            completed = data_tasks.join_next(), if !data_tasks.is_empty() => {
                if let Some(Ok(Some(message))) = completed {
                    let Ok(frame) = encode_control_frame(&message) else { return (registered, Instant::now()) };
                    if !send_control_message(&mut socket, Message::Binary(frame.into())).await {
                        return (registered, Instant::now());
                    }
                }
            }
            incoming = socket.next() => {
                let Some(Ok(message)) = incoming else { return (registered, Instant::now()) };
                match message {
                    Message::Binary(frame) => match decode_control_frame::<MainToEdge>(&frame) {
                        Ok(MainToEdge::HeartbeatAck { .. }) => registered = true,
                        Ok(MainToEdge::OpenDataChannel { session_id, target_id, expires_at_unix_ms }) => {
                            // Main can authorize a route immediately after accepting registration,
                            // before the first HeartbeatAck reaches this task. A valid command on
                            // the authenticated control socket also confirms registration.
                            registered = true;
                            data_tasks.spawn(run_data_channel(
                                config.clone(),
                                tls.clone(),
                                session_id,
                                target_id,
                                expires_at_unix_ms,
                                stop.clone(),
                            ));
                        }
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

async fn run_data_channel(
    config: EdgeConfig,
    tls: Arc<ClientConfig>,
    session_id: SessionId,
    target_id: String,
    expires_at_unix_ms: i64,
    stop: watch::Receiver<bool>,
) -> Option<EdgeToMain> {
    let failed = || EdgeToMain::DataChannelFailed { version: ProtocolVersion::current(), session_id };
    if unix_time_ms() > expires_at_unix_ms {
        return Some(failed());
    }
    let Some(target) = config.targets.get(&target_id) else { return Some(failed()) };
    match &target.address {
        TargetAddress::Tcp { tcp } => {
            let Ok(addresses) = TargetPolicy::new(target.allow_remote).resolve_and_validate(tcp).await else {
                return Some(failed());
            };
            let mut local = None;
            for address in addresses {
                if let Ok(Ok(stream)) = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(address)).await {
                    local = Some(stream);
                    break;
                }
            }
            let Some(local) = local else { return Some(failed()) };
            if !relay_data_channel(config, tls, session_id, target_id, local, stop).await {
                return Some(failed());
            }
        }
        TargetAddress::Unix { unix } => {
            #[cfg(unix)]
            if let Ok(Ok(local)) = tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::UnixStream::connect(unix)).await {
                if relay_data_channel(config, tls, session_id, target_id, local, stop).await {
                    return None;
                }
            }
            return Some(failed());
        }
    }
    None
}

async fn relay_data_channel<I>(
    config: EdgeConfig,
    tls: Arc<ClientConfig>,
    session_id: SessionId,
    target_id: String,
    local: I,
    stop: watch::Receiver<bool>,
) -> bool
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let Ok(Ok(mut socket)) = tokio::time::timeout(CONNECT_TIMEOUT, connect_data(&config, tls)).await else {
        return false;
    };
    let open = EdgeDataOpen { version: ProtocolVersion::current(), session_id, target_id };
    let Ok(frame) = encode_control_frame(&open) else { return false };
    if !send_control_message(&mut socket, Message::Binary(frame.into())).await {
        return false;
    }
    let _ = relay_websocket_to_io(socket, local, Duration::from_secs(300), stop).await;
    true
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

fn unix_time_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
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
    use super::{control_websocket_config, verify_server_pin};
    use crate::protocol::MAX_CONTROL_FRAME_SIZE;
    use rcgen::{CertificateParams, KeyPair};
    use sha2::{Digest, Sha256};
    use x509_parser::prelude::parse_x509_certificate;

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

    #[test]
    fn enrollment_rejects_the_wrong_main_spki_pin() {
        let key = KeyPair::generate().unwrap();
        let certificate = CertificateParams::default().self_signed(&key).unwrap();
        let chain = vec![certificate.der().clone()];
        let (_, parsed) = parse_x509_certificate(certificate.der()).unwrap();
        let expected = hex::encode(Sha256::digest(parsed.public_key().raw));

        assert!(verify_server_pin(Some(&chain), &expected).is_ok());
        assert!(verify_server_pin(Some(&chain), &"00".repeat(32)).is_err());
    }
}
