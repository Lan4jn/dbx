use std::collections::HashMap;
use std::io::Cursor;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dbx_gateway::protocol::{
    decode_control_frame, encode_control_frame, ClientEvent, ClientMessage, ProtocolVersion, Stage, MAX_DATA_FRAME_SIZE,
};
use futures::{SinkExt, StreamExt};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, Error as TlsError, RootCertStore, SignatureScheme};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, Mutex};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::models::connection::DbxGatewayConfig;

pub use dbx_gateway::protocol::{GatewayEdgeRoutes, GatewayRoute};

#[derive(Zeroize, ZeroizeOnDrop)]
pub struct GatewayClientIdentity {
    pub certificate_chain_der: Vec<Vec<u8>>,
    pub private_key_pkcs8_der: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayIdentityMetadata {
    pub id: String,
    pub name: String,
    pub subject: String,
    pub expires_at: String,
    pub fingerprint_sha256: String,
}

#[async_trait]
pub trait GatewayIdentityProvider: Send + Sync {
    async fn load(&self, identity_id: &str) -> Result<GatewayClientIdentity, String>;
}

#[derive(Default)]
pub struct UnavailableGatewayIdentityProvider;

#[async_trait]
impl GatewayIdentityProvider for UnavailableGatewayIdentityProvider {
    async fn load(&self, _identity_id: &str) -> Result<GatewayClientIdentity, String> {
        Err("DBX Gateway client identities are available only in the desktop application.".to_string())
    }
}

struct RunningGatewayTunnel {
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

pub struct DbxGatewayManager {
    identities: Arc<dyn GatewayIdentityProvider>,
    tunnels: Mutex<HashMap<String, RunningGatewayTunnel>>,
}

impl Default for DbxGatewayManager {
    fn default() -> Self {
        Self::new(Arc::new(UnavailableGatewayIdentityProvider))
    }
}

impl DbxGatewayManager {
    pub fn new(identities: Arc<dyn GatewayIdentityProvider>) -> Self {
        Self { identities, tunnels: Mutex::new(HashMap::new()) }
    }

    pub async fn start_tunnel(
        &self,
        layer_id: &str,
        dial_host: &str,
        dial_port: u16,
        config: &DbxGatewayConfig,
    ) -> Result<u16, String> {
        validate_config(config, true)?;
        self.stop_tunnel(layer_id).await;
        let mut probe = open_route_socket(self.identities.clone(), dial_host, dial_port, config).await?;
        let _ = probe.close(None).await;
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|error| format!("Could not start the local Gateway listener: {error}"))?;
        let port = listener.local_addr().map_err(|error| error.to_string())?.port();
        let identities = self.identities.clone();
        let dial_host = dial_host.to_string();
        let config = config.clone();
        let (shutdown, mut stop) = watch::channel(false);
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    changed = stop.changed() => {
                        if changed.is_err() || *stop.borrow() {
                            break;
                        }
                    }
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        let identities = identities.clone();
                        let dial_host = dial_host.clone();
                        let config = config.clone();
                        let stream_stop = stop.clone();
                        tokio::spawn(async move {
                            let _ = proxy_local_stream(stream, identities, &dial_host, dial_port, &config, stream_stop).await;
                        });
                    }
                }
            }
        });
        self.tunnels.lock().await.insert(layer_id.to_string(), RunningGatewayTunnel { shutdown, task });
        Ok(port)
    }

    pub async fn list_routes(&self, config: &DbxGatewayConfig) -> Result<Vec<GatewayEdgeRoutes>, String> {
        validate_config(config, false)?;
        let endpoint = GatewayEndpoint::parse(&config.main_url)?;
        let mut socket = connect_gateway(self.identities.clone(), &endpoint.host, endpoint.port, config).await?;
        expect_stage(&mut socket, Stage::MainAuthenticated).await?;
        send_control(
            &mut socket,
            &ClientMessage::ListRoutes { version: ProtocolVersion::current(), request_id: uuid::Uuid::new_v4() },
        )
        .await?;
        match next_event(&mut socket).await? {
            ClientEvent::Routes { edges } => Ok(edges),
            ClientEvent::Error { code } => Err(format!("Gateway route discovery failed: {code:?}")),
            ClientEvent::Stage { .. } => Err("Gateway returned an unexpected route discovery response".to_string()),
        }
    }

    pub async fn test_profile(&self, config: &DbxGatewayConfig) -> Result<String, String> {
        let routes = self.list_routes(config).await?;
        let online = routes.iter().filter(|edge| edge.online).count();
        Ok(format!("Main authenticated; {online} Edge gateway(s) online"))
    }

    pub async fn stop_tunnel(&self, layer_id: &str) {
        if let Some(running) = self.tunnels.lock().await.remove(layer_id) {
            let _ = running.shutdown.send(true);
            running.task.abort();
            let _ = running.task.await;
        }
    }
}

struct GatewayEndpoint {
    host: String,
    port: u16,
}

impl GatewayEndpoint {
    fn parse(main_url: &str) -> Result<Self, String> {
        let url = url::Url::parse(main_url).map_err(|_| "Gateway Main URL is invalid".to_string())?;
        if url.scheme() != "wss" {
            return Err("Gateway Main URL must use wss://".to_string());
        }
        let host = url.host_str().ok_or_else(|| "Gateway Main URL has no host".to_string())?.to_string();
        let port = url.port_or_known_default().ok_or_else(|| "Gateway Main URL has no port".to_string())?;
        Ok(Self { host, port })
    }

    fn server_name(&self) -> Result<ServerName<'static>, String> {
        if let Ok(ip) = self.host.trim_matches(['[', ']']).parse::<IpAddr>() {
            return Ok(ServerName::IpAddress(ip.into()));
        }
        ServerName::try_from(self.host.clone()).map_err(|_| "Gateway TLS server name is invalid".to_string())
    }
}

fn validate_config(config: &DbxGatewayConfig, require_route: bool) -> Result<(), String> {
    GatewayEndpoint::parse(&config.main_url)?;
    if config.identity_id.trim().is_empty() {
        return Err("Gateway client identity is required".to_string());
    }
    if config.server_ca_pem.trim().is_empty() && config.server_spki_sha256.trim().is_empty() {
        return Err("Gateway server CA or SPKI pin is required".to_string());
    }
    if require_route && (config.edge_id.trim().is_empty() || config.target_id.trim().is_empty()) {
        return Err("Gateway Edge and target route are required".to_string());
    }
    Ok(())
}

type GatewaySocket = tokio_tungstenite::WebSocketStream<tokio_rustls::client::TlsStream<TcpStream>>;

async fn connect_gateway(
    identities: Arc<dyn GatewayIdentityProvider>,
    dial_host: &str,
    dial_port: u16,
    config: &DbxGatewayConfig,
) -> Result<GatewaySocket, String> {
    let endpoint = GatewayEndpoint::parse(&config.main_url)?;
    let identity = identities.load(&config.identity_id).await?;
    let tls = build_tls_config(&identity, config)?;
    let server_name = endpoint.server_name()?;
    let connect_timeout = Duration::from_secs(config.connect_timeout_secs.max(1));
    let tcp = timeout(connect_timeout, TcpStream::connect((dial_host, dial_port)))
        .await
        .map_err(|_| "Gateway TCP connection timed out".to_string())?
        .map_err(|error| format!("Gateway TCP connection failed: {error}"))?;
    let tls = timeout(connect_timeout, TlsConnector::from(tls).connect(server_name, tcp))
        .await
        .map_err(|_| "Gateway TLS handshake timed out".to_string())?
        .map_err(|error| format!("Gateway TLS handshake failed: {error}"))?;
    verify_negotiated_pin(&tls, &config.server_spki_sha256)?;
    let request =
        config.main_url.as_str().into_client_request().map_err(|_| "Gateway WebSocket URL is invalid".to_string())?;
    timeout(connect_timeout, tokio_tungstenite::client_async(request, tls))
        .await
        .map_err(|_| "Gateway WebSocket handshake timed out".to_string())?
        .map(|(socket, _)| socket)
        .map_err(|error| format!("Gateway WebSocket handshake failed: {error}"))
}

fn build_tls_config(identity: &GatewayClientIdentity, config: &DbxGatewayConfig) -> Result<Arc<ClientConfig>, String> {
    let mut roots = RootCertStore::empty();
    if !config.server_ca_pem.trim().is_empty() {
        let mut reader = Cursor::new(config.server_ca_pem.as_bytes());
        let certificates = rustls_pemfile::certs(&mut reader)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| "Gateway server CA PEM is invalid".to_string())?;
        if certificates.is_empty() {
            return Err("Gateway server CA PEM contains no certificates".to_string());
        }
        for certificate in certificates {
            roots.add(certificate).map_err(|_| "Gateway server CA certificate is invalid".to_string())?;
        }
    }
    let certificates = identity.certificate_chain_der.iter().cloned().map(CertificateDer::from).collect::<Vec<_>>();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(identity.private_key_pkcs8_der.clone()));
    let builder = ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13]);
    let mut tls = if roots.is_empty() {
        let pin = normalized_pin(&config.server_spki_sha256)?
            .ok_or_else(|| "Gateway SPKI pin is required when no server CA is configured".to_string())?;
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(SpkiPinVerifier::new(pin)?))
            .with_client_auth_cert(certificates, key)
    } else {
        builder.with_root_certificates(roots).with_client_auth_cert(certificates, key)
    }
    .map_err(|_| "Gateway client certificate or private key is invalid".to_string())?;
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(tls))
}

fn normalized_pin(pin: &str) -> Result<Option<[u8; 32]>, String> {
    let pin = pin.trim().replace(':', "").to_ascii_lowercase();
    if pin.is_empty() {
        return Ok(None);
    }
    let bytes = hex::decode(pin).map_err(|_| "Gateway SPKI pin must be 64 hexadecimal characters".to_string())?;
    bytes.try_into().map(Some).map_err(|_| "Gateway SPKI pin must be 64 hexadecimal characters".to_string())
}

fn certificate_spki_sha256(certificate: &[u8]) -> Result<[u8; 32], String> {
    let (_, certificate) = x509_parser::parse_x509_certificate(certificate)
        .map_err(|_| "Gateway server certificate is invalid".to_string())?;
    Ok(Sha256::digest(certificate.tbs_certificate.subject_pki.raw).into())
}

fn verify_negotiated_pin(tls: &tokio_rustls::client::TlsStream<TcpStream>, configured_pin: &str) -> Result<(), String> {
    let Some(expected) = normalized_pin(configured_pin)? else { return Ok(()) };
    let certificate = tls
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|certificates| certificates.first())
        .ok_or_else(|| "Gateway server sent no certificate".to_string())?;
    if certificate_spki_sha256(certificate.as_ref())? != expected {
        return Err("Gateway server SPKI pin does not match".to_string());
    }
    Ok(())
}

#[derive(Debug)]
struct SpkiPinVerifier {
    pin: [u8; 32],
    algorithms: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl SpkiPinVerifier {
    fn new(pin: [u8; 32]) -> Result<Self, String> {
        let provider = CryptoProvider::get_default()
            .cloned()
            .unwrap_or_else(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()));
        Ok(Self { pin, algorithms: provider.signature_verification_algorithms })
    }
}

impl ServerCertVerifier for SpkiPinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let parsed = rustls::server::ParsedCertificate::try_from(end_entity)?;
        rustls::client::verify_server_name(&parsed, server_name)?;
        let (_, certificate) = x509_parser::parse_x509_certificate(end_entity.as_ref())
            .map_err(|_| TlsError::General("invalid pinned certificate".to_string()))?;
        if !certificate.validity().is_valid() {
            return Err(TlsError::General("pinned certificate is not currently valid".to_string()));
        }
        if certificate_spki_sha256(end_entity.as_ref()).map_err(TlsError::General)? != self.pin {
            return Err(TlsError::General("SPKI pin mismatch".to_string()));
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

async fn proxy_local_stream(
    local: TcpStream,
    identities: Arc<dyn GatewayIdentityProvider>,
    dial_host: &str,
    dial_port: u16,
    config: &DbxGatewayConfig,
    stop: watch::Receiver<bool>,
) -> Result<(), String> {
    let socket = open_route_socket(identities, dial_host, dial_port, config).await?;
    relay_local_websocket(local, socket, stop).await
}

async fn open_route_socket(
    identities: Arc<dyn GatewayIdentityProvider>,
    dial_host: &str,
    dial_port: u16,
    config: &DbxGatewayConfig,
) -> Result<GatewaySocket, String> {
    let mut socket = connect_gateway(identities, dial_host, dial_port, config).await?;
    expect_stage(&mut socket, Stage::MainAuthenticated).await?;
    send_control(
        &mut socket,
        &ClientMessage::OpenRoute {
            version: ProtocolVersion::current(),
            request_id: uuid::Uuid::new_v4(),
            edge_id: config.edge_id.clone(),
            target_id: config.target_id.clone(),
        },
    )
    .await?;
    loop {
        match next_event(&mut socket).await? {
            ClientEvent::Stage { stage: Stage::StreamReady } => break,
            ClientEvent::Stage { .. } => {}
            ClientEvent::Error { code } => return Err(format!("Gateway route failed: {code:?}")),
            ClientEvent::Routes { .. } => return Err("Gateway returned an unexpected route list".to_string()),
        }
    }
    Ok(socket)
}

async fn send_control(socket: &mut GatewaySocket, message: &ClientMessage) -> Result<(), String> {
    let frame = encode_control_frame(message).map_err(|error| error.message)?;
    socket.send(Message::Binary(frame.into())).await.map_err(|error| format!("Gateway send failed: {error}"))
}

async fn expect_stage(socket: &mut GatewaySocket, expected: Stage) -> Result<(), String> {
    match next_event(socket).await? {
        ClientEvent::Stage { stage } if stage == expected => Ok(()),
        ClientEvent::Error { code } => Err(format!("Gateway authentication failed: {code:?}")),
        _ => Err("Gateway returned an unexpected authentication response".to_string()),
    }
}

async fn next_event(socket: &mut GatewaySocket) -> Result<ClientEvent, String> {
    match socket.next().await {
        Some(Ok(Message::Binary(frame))) => decode_control_frame(&frame).map_err(|error| error.message),
        Some(Ok(Message::Close(_))) | None => Err("Gateway closed the connection".to_string()),
        Some(Ok(_)) => Err("Gateway returned an unexpected WebSocket frame".to_string()),
        Some(Err(error)) => Err(format!("Gateway receive failed: {error}")),
    }
}

async fn relay_local_websocket(
    local: TcpStream,
    socket: GatewaySocket,
    mut stop: watch::Receiver<bool>,
) -> Result<(), String> {
    let (mut local_reader, mut local_writer) = local.into_split();
    let (mut socket_writer, mut socket_reader) = socket.split();
    let mut buffer = vec![0_u8; MAX_DATA_FRAME_SIZE];
    loop {
        tokio::select! {
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() { break; }
            }
            read = local_reader.read(&mut buffer) => {
                let read = read.map_err(|error| format!("Local Gateway stream read failed: {error}"))?;
                if read == 0 { break; }
                socket_writer
                    .send(Message::Binary(buffer[..read].to_vec().into()))
                    .await
                    .map_err(|error| format!("Gateway stream send failed: {error}"))?;
            }
            message = socket_reader.next() => match message {
                Some(Ok(Message::Binary(data))) => local_writer
                    .write_all(&data)
                    .await
                    .map_err(|error| format!("Local Gateway stream write failed: {error}"))?,
                Some(Ok(Message::Ping(data))) => socket_writer
                    .send(Message::Pong(data))
                    .await
                    .map_err(|error| format!("Gateway keepalive failed: {error}"))?,
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(format!("Gateway stream receive failed: {error}")),
            }
        }
    }
    let _ = socket_writer.close().await;
    Ok(())
}

#[cfg(all(test, not(windows)))]
mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Mutex;

    use dbx_gateway::config::{EdgeConfig, EdgeTarget, MainConfig, TargetAddress};
    use dbx_gateway::edge_gateway::EdgeGateway;
    use dbx_gateway::main_gateway::MainGateway;
    use rcgen::{
        BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose,
        SanType,
    };
    use time::{Duration as TimeDuration, OffsetDateTime};
    use tokio::task::JoinHandle;

    use super::*;

    const EDGE_ID: &str = "edge-manager-test";
    const TARGET_ID: &str = "echo";

    fn gateway_config() -> DbxGatewayConfig {
        DbxGatewayConfig {
            id: "gateway-layer".to_string(),
            name: "Gateway".to_string(),
            enabled: true,
            profile_id: "gateway-profile".to_string(),
            main_url: "wss://localhost:443/_dbx/client".to_string(),
            identity_id: "identity-1".to_string(),
            server_ca_pem: String::new(),
            server_spki_sha256: "11".repeat(32),
            connect_timeout_secs: 1,
            edge_id: "edge-prod-01".to_string(),
            target_id: "postgres-primary".to_string(),
            use_as_connection_info: true,
        }
    }

    #[test]
    fn gateway_ip_endpoints_use_ip_tls_server_names() {
        for url in ["wss://192.0.2.53/_dbx/client", "wss://[2001:db8::53]/_dbx/client"] {
            let endpoint = GatewayEndpoint::parse(url).unwrap();
            assert!(matches!(endpoint.server_name().unwrap(), ServerName::IpAddress(_)), "{url}");
        }
    }

    #[test]
    fn gateway_dns_endpoint_uses_a_dns_tls_server_name() {
        let endpoint = GatewayEndpoint::parse("wss://gateway.example.com/_dbx/client").unwrap();
        assert!(matches!(endpoint.server_name().unwrap(), ServerName::DnsName(_)));
    }

    type MemoryIdentity = (Vec<Vec<u8>>, Vec<u8>);

    struct MemoryIdentityProvider {
        identities: Mutex<HashMap<String, MemoryIdentity>>,
    }

    #[async_trait]
    impl GatewayIdentityProvider for MemoryIdentityProvider {
        async fn load(&self, identity_id: &str) -> Result<GatewayClientIdentity, String> {
            let identities = self.identities.lock().map_err(|_| "identity store is unavailable".to_string())?;
            let (certificate_chain_der, private_key_pkcs8_der) = identities
                .get(identity_id)
                .cloned()
                .ok_or_else(|| format!("Gateway identity '{identity_id}' was not found"))?;
            Ok(GatewayClientIdentity { certificate_chain_der, private_key_pkcs8_der })
        }
    }

    #[tokio::test]
    async fn identity_provider_resolves_only_the_requested_identity() {
        let provider = MemoryIdentityProvider {
            identities: Mutex::new(HashMap::from([("identity-1".to_string(), (vec![vec![1, 2, 3]], vec![4, 5, 6]))])),
        };

        let identity = provider.load("identity-1").await.unwrap();
        assert_eq!(identity.certificate_chain_der, vec![vec![1, 2, 3]]);
        assert_eq!(identity.private_key_pkcs8_der, vec![4, 5, 6]);
        assert!(provider.load("identity-2").await.is_err());
    }

    #[tokio::test]
    async fn identity_unavailable_provider_fails_closed() {
        let error = match UnavailableGatewayIdentityProvider.load("identity-1").await {
            Ok(_) => panic!("unavailable provider unexpectedly returned an identity"),
            Err(error) => error,
        };
        assert!(error.contains("desktop"));
    }

    #[tokio::test]
    async fn manager_rejects_insecure_urls_and_missing_routes() {
        let manager = DbxGatewayManager::default();
        let mut config = gateway_config();
        config.main_url = "ws://localhost/_dbx/client".to_string();
        assert!(manager.start_tunnel("layer", "localhost", 443, &config).await.unwrap_err().contains("wss://"));

        config.main_url = "wss://localhost/_dbx/client".to_string();
        config.target_id.clear();
        assert!(manager.start_tunnel("layer", "localhost", 443, &config).await.unwrap_err().contains("route"));
    }

    #[tokio::test]
    async fn manager_lists_online_logical_routes_over_mtls() {
        let rig = GatewayTestRig::start().await;
        let manager = rig.manager();

        let edges = manager.list_routes(&rig.config).await.unwrap();

        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].edge_id, EDGE_ID);
        assert!(edges[0].online);
        assert_eq!(edges[0].routes.len(), 1);
        assert_eq!(edges[0].routes[0].target_id, TARGET_ID);
        assert_eq!(edges[0].routes[0].display_name, "Echo");
        rig.shutdown().await;
    }

    #[tokio::test]
    async fn manager_round_trips_random_binary_and_stop_releases_listener() {
        let rig = GatewayTestRig::start().await;
        let manager = rig.manager();
        let port =
            manager.start_tunnel("manager-e2e", "127.0.0.1", rig.main.local_addr().port(), &rig.config).await.unwrap();
        let mut stream = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).await.unwrap();
        let payload = (0..4096).flat_map(|_| *uuid::Uuid::new_v4().as_bytes()).collect::<Vec<_>>();

        stream.write_all(&payload).await.unwrap();
        let mut echoed = vec![0; payload.len()];
        timeout(Duration::from_secs(5), stream.read_exact(&mut echoed)).await.unwrap().unwrap();
        assert_eq!(echoed, payload);
        drop(stream);

        manager.stop_tunnel("manager-e2e").await;
        let rebound = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await.unwrap();
        assert_eq!(rebound.local_addr().unwrap().port(), port);
        rig.shutdown().await;
    }

    #[tokio::test]
    async fn manager_rejects_an_incorrect_server_spki_pin() {
        let rig = GatewayTestRig::start().await;
        let manager = rig.manager();
        let mut config = rig.config.clone();
        config.server_spki_sha256 = "00".repeat(32);

        let error = manager
            .start_tunnel("manager-bad-pin", "127.0.0.1", rig.main.local_addr().port(), &config)
            .await
            .unwrap_err();

        assert!(error.contains("SPKI pin does not match"), "unexpected error: {error}");
        rig.shutdown().await;
    }

    struct TestCa {
        certificate_der: Vec<u8>,
        certificate_pem: String,
        issuer: Issuer<'static, KeyPair>,
    }

    struct IssuedIdentity {
        certificate_chain_der: Vec<Vec<u8>>,
        private_key_der: Vec<u8>,
        certificate_pem: String,
        private_key_pem: String,
    }

    struct GatewayTestRig {
        _dir: tempfile::TempDir,
        main: MainGateway,
        edge: EdgeGateway,
        echo_task: JoinHandle<()>,
        config: DbxGatewayConfig,
        identities: Arc<MemoryIdentityProvider>,
    }

    impl GatewayTestRig {
        async fn start() -> Self {
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
            let dir = tempfile::tempdir().unwrap();
            let canonical_dir = fs::canonicalize(dir.path()).unwrap();
            let server_ca = make_ca("manager-server-ca");
            let edge_ca = make_ca("manager-edge-ca");
            let client_ca = make_ca("manager-client-ca");
            let server = issue_server(&server_ca.issuer);
            let edge_identity = issue_client(&edge_ca, &format!("urn:dbx-gateway:edge:{EDGE_ID}"));
            let client_identity = issue_client(&client_ca, "urn:dbx-gateway:client:manager-test");

            let server_certificate = write_file(&canonical_dir, "server.pem", &server.certificate_pem, false);
            let server_key = write_file(&canonical_dir, "server.key", &server.private_key_pem, true);
            let edge_ca_certificate = write_file(&canonical_dir, "edge-ca.pem", &edge_ca.certificate_pem, false);
            let client_ca_certificate = write_file(&canonical_dir, "client-ca.pem", &client_ca.certificate_pem, false);
            let server_ca_certificate = write_file(&canonical_dir, "server-ca.pem", &server_ca.certificate_pem, false);
            let edge_certificate = write_file(&canonical_dir, "edge.pem", &edge_identity.certificate_pem, false);
            let edge_key = write_file(&canonical_dir, "edge.key", &edge_identity.private_key_pem, true);

            let main = MainGateway::bind(MainConfig {
                listen: "127.0.0.1:0".to_string(),
                certificate: server_certificate,
                private_key: server_key,
                edge_ca_certificate,
                client_ca_certificate,
                edge_path: "/_dbx/edge".to_string(),
                dbx_path: "/_dbx/client".to_string(),
                max_connections: 32,
                tls_handshake_timeout_secs: 5,
                http_header_timeout_secs: 5,
                enrollment: None,
                allowed_edge_ids: Vec::new(),
                revoked_edge_serials: Vec::new(),
                client_route_acl: BTreeMap::new(),
                fallback_upstream: None,
                health_listen: None,
                state_file: None,
                max_streams_per_edge: 8,
                max_streams_per_client: 8,
                connection_rate_per_second: 64,
                connection_rate_burst: 64,
                global_buffer_budget_bytes: 16 * 1024 * 1024,
            })
            .await
            .unwrap();

            let echo = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let echo_address = echo.local_addr().unwrap();
            let echo_task = tokio::spawn(run_echo_server(echo));
            let edge = EdgeGateway::start(EdgeConfig {
                edge_id: EDGE_ID.to_string(),
                main_url: format!("wss://localhost:{}/_dbx/edge", main.local_addr().port()),
                certificate: edge_certificate,
                private_key: edge_key,
                ca_certificate: server_ca_certificate,
                targets: BTreeMap::from([(
                    TARGET_ID.to_string(),
                    EdgeTarget {
                        display_name: "Echo".to_string(),
                        address: TargetAddress::Tcp { tcp: echo_address.to_string() },
                        allow_remote: false,
                    },
                )]),
                bootstrap: None,
            })
            .unwrap();
            wait_for_edge(&main).await;

            let config = DbxGatewayConfig {
                id: "gateway-layer".to_string(),
                name: "Gateway".to_string(),
                enabled: true,
                profile_id: "gateway-profile".to_string(),
                main_url: format!("wss://localhost:{}/_dbx/client", main.local_addr().port()),
                identity_id: "identity-1".to_string(),
                server_ca_pem: server_ca.certificate_pem,
                server_spki_sha256: String::new(),
                connect_timeout_secs: 5,
                edge_id: EDGE_ID.to_string(),
                target_id: TARGET_ID.to_string(),
                use_as_connection_info: true,
            };
            let identities = Arc::new(MemoryIdentityProvider {
                identities: Mutex::new(HashMap::from([(
                    "identity-1".to_string(),
                    (client_identity.certificate_chain_der, client_identity.private_key_der),
                )])),
            });
            Self { _dir: dir, main, edge, echo_task, config, identities }
        }

        fn manager(&self) -> DbxGatewayManager {
            DbxGatewayManager::new(self.identities.clone())
        }

        async fn shutdown(self) {
            self.edge.shutdown().await;
            self.main.shutdown().await;
            self.echo_task.abort();
            let _ = self.echo_task.await;
        }
    }

    async fn run_echo_server(listener: TcpListener) {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let (mut reader, mut writer) = stream.split();
                let _ = tokio::io::copy(&mut reader, &mut writer).await;
            });
        }
    }

    async fn wait_for_edge(main: &MainGateway) {
        timeout(Duration::from_secs(5), async {
            loop {
                if main.registry().read().await.get(EDGE_ID).is_some_and(|edge| edge.online) {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    fn make_ca(name: &str) -> TestCa {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::default();
        params.distinguished_name.push(DnType::CommonName, name);
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::DigitalSignature];
        let certificate = params.self_signed(&key).unwrap();
        TestCa {
            certificate_der: certificate.der().to_vec(),
            certificate_pem: certificate.pem(),
            issuer: Issuer::new(params, key),
        }
    }

    fn issue_server(issuer: &Issuer<'_, KeyPair>) -> IssuedIdentity {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::default();
        params.distinguished_name.push(DnType::CommonName, "localhost");
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.subject_alt_names = vec![SanType::DnsName("localhost".try_into().unwrap())];
        let certificate = params.signed_by(&key, issuer).unwrap();
        IssuedIdentity {
            certificate_chain_der: vec![certificate.der().to_vec()],
            private_key_der: key.serialize_der(),
            certificate_pem: certificate.pem(),
            private_key_pem: key.serialize_pem(),
        }
    }

    fn issue_client(ca: &TestCa, uri: &str) -> IssuedIdentity {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::default();
        params.distinguished_name.push(DnType::CommonName, "gateway-peer");
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        params.subject_alt_names = vec![SanType::URI(uri.try_into().unwrap())];
        let now = OffsetDateTime::now_utc();
        params.not_before = now - TimeDuration::minutes(5);
        params.not_after = now + TimeDuration::days(1);
        let certificate = params.signed_by(&key, &ca.issuer).unwrap();
        IssuedIdentity {
            certificate_chain_der: vec![certificate.der().to_vec(), ca.certificate_der.clone()],
            private_key_der: key.serialize_der(),
            certificate_pem: format!("{}{}", certificate.pem(), ca.certificate_pem),
            private_key_pem: key.serialize_pem(),
        }
    }

    fn write_file(dir: &std::path::Path, name: &str, contents: &str, private: bool) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, contents).unwrap();
        #[cfg(unix)]
        if private {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        path
    }
}
