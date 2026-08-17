use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, ServerName};
use rustls::server::WebPkiClientVerifier;
use rustls::{ClientConfig, ServerConfig};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};
use tokio::sync::watch;
use tokio_rustls::{TlsAcceptor, TlsConnector};
use x509_parser::extensions::GeneralName;
use x509_parser::prelude::parse_x509_certificate;
use zeroize::Zeroizing;

use super::{EdgeIssueRequest, PkiStore};
use crate::state::GatewayState;
use crate::tls::{load_certificates, load_private_key, load_roots};
use crate::{GatewayError, GatewayErrorCode};

const MAX_ENROLLMENT_REQUEST_BYTES: u64 = 256 * 1024;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EnrollCsrRequest {
    pub token: Zeroizing<String>,
    pub claimed_edge_id: String,
    pub csr_der: Vec<u8>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct EnrollCsrResponse {
    pub edge_id: String,
    pub serial_hex: String,
    pub certificate_pem: String,
    pub chain_pem: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RenewCsrRequest {
    pub edge_id: String,
    pub current_serial: String,
    pub csr_der: Vec<u8>,
}

#[derive(Clone)]
pub struct PkiEnrollmentService {
    state: GatewayState,
    store: PkiStore,
    ca_password: Arc<Zeroizing<String>>,
}

impl PkiEnrollmentService {
    pub fn new(state: GatewayState, store: PkiStore, ca_password: Zeroizing<String>) -> Self {
        Self { state, store, ca_password: Arc::new(ca_password) }
    }

    pub async fn renew(&self, request: RenewCsrRequest) -> Result<EnrollCsrResponse, GatewayError> {
        if request.csr_der.len() as u64 > MAX_ENROLLMENT_REQUEST_BYTES
            || !self.state.certificate_is_active(&request.edge_id, &request.current_serial).await?
        {
            return Err(service_error(GatewayErrorCode::RouteDenied, "renewal request rejected"));
        }
        let store = self.store.clone();
        let password = self.ca_password.clone();
        let edge_id = request.edge_id.clone();
        let csr_der = request.csr_der;
        let issued = tokio::task::spawn_blocking(move || {
            store.issue_edge(
                EdgeIssueRequest { edge_id: &edge_id, csr_der: &csr_der, validity: time::Duration::days(90) },
                &password,
            )
        })
        .await
        .map_err(|_| service_error(GatewayErrorCode::Internal, "edge certificate could not be renewed"))??;
        self.state.rotate_issued_certificate(&request.edge_id, &request.current_serial, &issued.serial_hex).await?;
        Ok(EnrollCsrResponse {
            edge_id: request.edge_id,
            serial_hex: issued.serial_hex,
            certificate_pem: issued.certificate_pem,
            chain_pem: issued.chain_pem,
        })
    }

    pub async fn enroll(&self, request: EnrollCsrRequest) -> Result<EnrollCsrResponse, GatewayError> {
        if request.csr_der.len() as u64 > MAX_ENROLLMENT_REQUEST_BYTES {
            return Err(service_error(GatewayErrorCode::CapacityExceeded, "enrollment request rejected"));
        }
        let enrollment = self
            .state
            .enrollments
            .consume(&request.claimed_edge_id, request.token.as_str())
            .await
            .map_err(|_| service_error(GatewayErrorCode::RouteDenied, "enrollment request rejected"))?;
        let store = self.store.clone();
        let password = self.ca_password.clone();
        let edge_id = enrollment.edge_id;
        let csr_der = request.csr_der;
        let issued = tokio::task::spawn_blocking(move || {
            store.issue_edge(
                EdgeIssueRequest { edge_id: &edge_id, csr_der: &csr_der, validity: time::Duration::days(90) },
                &password,
            )
        })
        .await
        .map_err(|_| service_error(GatewayErrorCode::Internal, "edge certificate could not be issued"))??;
        self.state.record_issued_certificate(&request.claimed_edge_id, &issued.serial_hex).await?;
        Ok(EnrollCsrResponse {
            edge_id: request.claimed_edge_id,
            serial_hex: issued.serial_hex,
            certificate_pem: issued.certificate_pem,
            chain_pem: issued.chain_pem,
        })
    }
}

pub struct UnixPkiServer {
    path: PathBuf,
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Clone)]
pub struct RemotePkiConfig {
    pub listen: String,
    pub certificate: PathBuf,
    pub private_key: PathBuf,
    pub main_ra_ca_certificate: PathBuf,
    pub allowed_ra_uri_sans: Vec<String>,
}

pub struct RemotePkiServer {
    address: std::net::SocketAddr,
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl RemotePkiServer {
    pub fn local_addr(&self) -> std::net::SocketAddr {
        self.address
    }

    pub async fn shutdown(self) {
        let _ = self.stop.send(true);
        let _ = self.task.await;
    }
}

impl UnixPkiServer {
    pub async fn shutdown(self) {
        let _ = self.stop.send(true);
        let _ = self.task.await;
        let _ = std::fs::remove_file(self.path);
    }
}

pub async fn serve_unix(
    path: PathBuf,
    allowed_uid: u32,
    allowed_gid: u32,
    service: PkiEnrollmentService,
) -> Result<UnixPkiServer, GatewayError> {
    reject_existing_socket_path(&path)?;
    let listener = UnixListener::bind(&path)
        .map_err(|_| service_error(GatewayErrorCode::ConfigInvalid, "PKI Unix socket could not be bound"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o660))
            .map_err(|_| service_error(GatewayErrorCode::Internal, "PKI Unix socket permissions could not be set"))?;
    }
    let (stop, mut stopping) = watch::channel(false);
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                result = listener.accept() => {
                    let Ok((stream, _)) = result else { break };
                    let service = service.clone();
                    tokio::spawn(async move {
                        let _ = handle_unix(stream, allowed_uid, allowed_gid, service).await;
                    });
                }
                changed = stopping.changed() => {
                    if changed.is_err() || *stopping.borrow() { break; }
                }
            }
        }
    });
    Ok(UnixPkiServer { path, stop, task })
}

pub async fn serve_remote(
    config: RemotePkiConfig,
    service: PkiEnrollmentService,
) -> Result<RemotePkiServer, GatewayError> {
    if config.allowed_ra_uri_sans.is_empty()
        || config.allowed_ra_uri_sans.iter().any(|uri| !uri.starts_with("urn:dbx-gateway:ra:"))
    {
        return Err(service_error(GatewayErrorCode::ConfigInvalid, "remote PKI RA URI allowlist is invalid"));
    }
    let verifier = WebPkiClientVerifier::builder(Arc::new(load_roots(&config.main_ra_ca_certificate)?))
        .build()
        .map_err(|_| service_error(GatewayErrorCode::ConfigInvalid, "remote PKI RA CA is invalid"))?;
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let server_config = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|_| service_error(GatewayErrorCode::ConfigInvalid, "remote PKI TLS configuration is invalid"))?
        .with_client_cert_verifier(verifier)
        .with_single_cert(load_certificates(&config.certificate)?, load_private_key(&config.private_key)?)
        .map_err(|_| service_error(GatewayErrorCode::ConfigInvalid, "remote PKI certificate is invalid"))?;
    let listener = TcpListener::bind(&config.listen)
        .await
        .map_err(|_| service_error(GatewayErrorCode::ConfigInvalid, "remote PKI listener could not be bound"))?;
    let address = listener
        .local_addr()
        .map_err(|_| service_error(GatewayErrorCode::Internal, "remote PKI listener address is unavailable"))?;
    let acceptor = TlsAcceptor::from(Arc::new(server_config));
    let allowed_ra_uri_sans = Arc::new(config.allowed_ra_uri_sans);
    let (stop, mut stopping) = watch::channel(false);
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                result = listener.accept() => {
                    let Ok((stream, _)) = result else { break };
                    let acceptor = acceptor.clone();
                    let service = service.clone();
                    let allowed = allowed_ra_uri_sans.clone();
                    tokio::spawn(async move {
                        let Ok(stream) = acceptor.accept(stream).await else { return };
                        let certificates = stream.get_ref().1.peer_certificates();
                        if validate_ra_identity(certificates, &allowed).is_err() { return; }
                        let _ = handle_stream(stream, service).await;
                    });
                }
                changed = stopping.changed() => {
                    if changed.is_err() || *stopping.borrow() { break; }
                }
            }
        }
    });
    Ok(RemotePkiServer { address, stop, task })
}

pub async fn enroll_over_unix(path: &Path, request: &EnrollCsrRequest) -> Result<EnrollCsrResponse, GatewayError> {
    let stream = UnixStream::connect(path)
        .await
        .map_err(|_| service_error(GatewayErrorCode::EdgeOffline, "PKI service unavailable"))?;
    exchange(stream, &ServiceRequest::Enroll { request }).await
}

pub async fn renew_over_unix(path: &Path, request: &RenewCsrRequest) -> Result<EnrollCsrResponse, GatewayError> {
    let stream = UnixStream::connect(path)
        .await
        .map_err(|_| service_error(GatewayErrorCode::EdgeOffline, "PKI service unavailable"))?;
    exchange(stream, &ServiceRequest::Renew { request }).await
}

pub async fn enroll_over_remote(
    address: std::net::SocketAddr,
    server_name: &str,
    ca_certificate: &Path,
    certificate: &Path,
    private_key: &Path,
    request: &EnrollCsrRequest,
) -> Result<EnrollCsrResponse, GatewayError> {
    let client = ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .with_root_certificates(load_roots(ca_certificate)?)
        .with_client_auth_cert(load_certificates(certificate)?, load_private_key(private_key)?)
        .map_err(|_| service_error(GatewayErrorCode::ConfigInvalid, "PKI RA identity is invalid"))?;
    let stream = TcpStream::connect(address)
        .await
        .map_err(|_| service_error(GatewayErrorCode::EdgeOffline, "PKI service unavailable"))?;
    let name = ServerName::try_from(server_name.to_string())
        .map_err(|_| service_error(GatewayErrorCode::ConfigInvalid, "PKI server name is invalid"))?;
    let stream = TlsConnector::from(Arc::new(client))
        .connect(name, stream)
        .await
        .map_err(|_| service_error(GatewayErrorCode::IdentityRejected, "PKI service TLS rejected"))?;
    exchange(stream, &ServiceRequest::Enroll { request }).await
}

pub async fn renew_over_remote(
    address: std::net::SocketAddr,
    server_name: &str,
    ca_certificate: &Path,
    certificate: &Path,
    private_key: &Path,
    request: &RenewCsrRequest,
) -> Result<EnrollCsrResponse, GatewayError> {
    let client = ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .with_root_certificates(load_roots(ca_certificate)?)
        .with_client_auth_cert(load_certificates(certificate)?, load_private_key(private_key)?)
        .map_err(|_| service_error(GatewayErrorCode::ConfigInvalid, "PKI RA identity is invalid"))?;
    let stream = TcpStream::connect(address)
        .await
        .map_err(|_| service_error(GatewayErrorCode::EdgeOffline, "PKI service unavailable"))?;
    let name = ServerName::try_from(server_name.to_string())
        .map_err(|_| service_error(GatewayErrorCode::ConfigInvalid, "PKI server name is invalid"))?;
    let stream = TlsConnector::from(Arc::new(client))
        .connect(name, stream)
        .await
        .map_err(|_| service_error(GatewayErrorCode::IdentityRejected, "PKI service TLS rejected"))?;
    exchange(stream, &ServiceRequest::Renew { request }).await
}

async fn handle_unix(
    stream: UnixStream,
    allowed_uid: u32,
    allowed_gid: u32,
    service: PkiEnrollmentService,
) -> Result<(), GatewayError> {
    let credentials =
        stream.peer_cred().map_err(|_| service_error(GatewayErrorCode::IdentityRejected, "PKI peer rejected"))?;
    if credentials.uid() != allowed_uid || credentials.gid() != allowed_gid {
        return Err(service_error(GatewayErrorCode::IdentityRejected, "PKI peer rejected"));
    }
    handle_stream(stream, service).await
}

async fn handle_stream<S>(mut stream: S, service: PkiEnrollmentService) -> Result<(), GatewayError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let body = read_frame(&mut stream).await?;
    let result = match serde_json::from_slice::<OwnedServiceRequest>(&body) {
        Ok(OwnedServiceRequest::Enroll { request }) => service.enroll(request).await,
        Ok(OwnedServiceRequest::Renew { request }) => service.renew(request).await,
        Err(_) => Err(service_error(GatewayErrorCode::ProtocolMismatch, "enrollment request rejected")),
    };
    let response = match result {
        Ok(response) => WireResponse::Ok { response },
        Err(error) => WireResponse::Error { code: error.code },
    };
    let body = serde_json::to_vec(&response)
        .map_err(|_| service_error(GatewayErrorCode::Internal, "enrollment response could not be encoded"))?;
    write_frame(&mut stream, &body).await
}

async fn exchange<S>(mut stream: S, request: &ServiceRequest<'_>) -> Result<EnrollCsrResponse, GatewayError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let body = serde_json::to_vec(request)
        .map_err(|_| service_error(GatewayErrorCode::Internal, "enrollment request could not be encoded"))?;
    write_frame(&mut stream, &body).await?;
    let response = read_frame(&mut stream).await?;
    let response: WireResponse = serde_json::from_slice(&response)
        .map_err(|_| service_error(GatewayErrorCode::Internal, "PKI service returned an invalid response"))?;
    match response {
        WireResponse::Ok { response } => Ok(response),
        WireResponse::Error { code } => Err(service_error(code, "enrollment request rejected")),
    }
}

#[derive(Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum ServiceRequest<'a> {
    Enroll { request: &'a EnrollCsrRequest },
    Renew { request: &'a RenewCsrRequest },
}

#[derive(Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum OwnedServiceRequest {
    Enroll { request: EnrollCsrRequest },
    Renew { request: RenewCsrRequest },
}

async fn read_frame<S>(stream: &mut S) -> Result<Vec<u8>, GatewayError>
where
    S: AsyncRead + Unpin,
{
    let length = stream
        .read_u32()
        .await
        .map_err(|_| service_error(GatewayErrorCode::ProtocolMismatch, "enrollment frame rejected"))?;
    if u64::from(length) > MAX_ENROLLMENT_REQUEST_BYTES {
        return Err(service_error(GatewayErrorCode::CapacityExceeded, "enrollment frame rejected"));
    }
    let mut body = vec![0_u8; length as usize];
    stream
        .read_exact(&mut body)
        .await
        .map_err(|_| service_error(GatewayErrorCode::ProtocolMismatch, "enrollment frame rejected"))?;
    Ok(body)
}

async fn write_frame<S>(stream: &mut S, body: &[u8]) -> Result<(), GatewayError>
where
    S: AsyncWrite + Unpin,
{
    let length = u32::try_from(body.len())
        .ok()
        .filter(|length| u64::from(*length) <= MAX_ENROLLMENT_REQUEST_BYTES)
        .ok_or_else(|| service_error(GatewayErrorCode::CapacityExceeded, "enrollment frame rejected"))?;
    stream
        .write_u32(length)
        .await
        .map_err(|_| service_error(GatewayErrorCode::EdgeOffline, "PKI service unavailable"))?;
    stream
        .write_all(body)
        .await
        .map_err(|_| service_error(GatewayErrorCode::EdgeOffline, "PKI service unavailable"))?;
    stream.flush().await.map_err(|_| service_error(GatewayErrorCode::EdgeOffline, "PKI service unavailable"))
}

fn validate_ra_identity(
    certificates: Option<&[CertificateDer<'static>]>,
    allowed_uri_sans: &[String],
) -> Result<(), GatewayError> {
    let leaf = certificates
        .and_then(|certificates| certificates.first())
        .ok_or_else(|| service_error(GatewayErrorCode::IdentityRejected, "PKI RA identity rejected"))?;
    let (_, certificate) = parse_x509_certificate(leaf.as_ref())
        .map_err(|_| service_error(GatewayErrorCode::IdentityRejected, "PKI RA identity rejected"))?;
    let eku = certificate
        .extended_key_usage()
        .map_err(|_| service_error(GatewayErrorCode::IdentityRejected, "PKI RA identity rejected"))?
        .ok_or_else(|| service_error(GatewayErrorCode::IdentityRejected, "PKI RA identity rejected"))?;
    if !eku.value.client_auth || eku.value.server_auth || eku.value.any {
        return Err(service_error(GatewayErrorCode::IdentityRejected, "PKI RA identity rejected"));
    }
    let sans = certificate
        .subject_alternative_name()
        .map_err(|_| service_error(GatewayErrorCode::IdentityRejected, "PKI RA identity rejected"))?
        .ok_or_else(|| service_error(GatewayErrorCode::IdentityRejected, "PKI RA identity rejected"))?;
    let uris = sans
        .value
        .general_names
        .iter()
        .filter_map(|name| match name {
            GeneralName::URI(uri) => Some(*uri),
            _ => None,
        })
        .collect::<Vec<_>>();
    if uris.len() != 1 || !allowed_uri_sans.iter().any(|allowed| allowed == uris[0]) {
        return Err(service_error(GatewayErrorCode::IdentityRejected, "PKI RA identity rejected"));
    }
    Ok(())
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum WireResponse {
    Ok { response: EnrollCsrResponse },
    Error { code: GatewayErrorCode },
}

fn reject_existing_socket_path(path: &Path) -> Result<(), GatewayError> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        _ => Err(service_error(GatewayErrorCode::ConfigInvalid, "PKI Unix socket path already exists")),
    }
}

fn service_error(code: GatewayErrorCode, message: impl Into<String>) -> GatewayError {
    GatewayError { code, message: message.into() }
}
