#![cfg(feature = "server")]

use std::collections::BTreeMap;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use dbx_gateway::config::{EdgeConfig, EdgeTarget, MainConfig, TargetAddress};
use dbx_gateway::edge_gateway::{EdgeGateway, ReconnectBackoff};
use dbx_gateway::main_gateway::MainGateway;
use dbx_gateway::protocol::{encode_control_frame, EdgeRegistration, ProtocolVersion, RegisteredTarget};
use futures_util::{SinkExt, StreamExt};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    SanType,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
use time::{Duration, OffsetDateTime};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{advance, sleep, timeout, Instant};
use tokio_rustls::TlsConnector;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async_tls_with_config, Connector, MaybeTlsStream, WebSocketStream};

const EDGE_PATH: &str = "/_dbx/edge";
const DBX_PATH: &str = "/_dbx/client";
static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        loop {
            let id = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("dbx-gateway-tls-{}-{id}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Self(fs::canonicalize(path).unwrap()),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("failed to create test directory: {error}"),
            }
        }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct TestCa {
    certificate: CertificateDer<'static>,
    certificate_pem: String,
    issuer: Issuer<'static, KeyPair>,
}

struct ClientIdentity {
    certificates: Vec<CertificateDer<'static>>,
    private_key: PrivateKeyDer<'static>,
    certificate_pem: String,
    private_key_pem: String,
}

struct Fixture {
    _dir: TempDir,
    config: MainConfig,
    server_ca: CertificateDer<'static>,
    server_ca_certificate: PathBuf,
    edge_ca: TestCa,
    client_ca: TestCa,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new();
        let server_ca = make_ca("server-ca");
        let edge_ca = make_ca("edge-ca");
        let client_ca = make_ca("client-ca");
        let (server_certificate, server_key) = issue_server(&server_ca.issuer);

        let certificate = dir.0.join("server.pem");
        let private_key = dir.0.join("server.key");
        let edge_ca_certificate = dir.0.join("edge-ca.pem");
        let client_ca_certificate = dir.0.join("client-ca.pem");
        let server_ca_certificate = dir.0.join("server-ca.pem");
        fs::write(&certificate, server_certificate).unwrap();
        fs::write(&private_key, server_key).unwrap();
        fs::write(&edge_ca_certificate, &edge_ca.certificate_pem).unwrap();
        fs::write(&client_ca_certificate, &client_ca.certificate_pem).unwrap();
        fs::write(&server_ca_certificate, &server_ca.certificate_pem).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&private_key, fs::Permissions::from_mode(0o600)).unwrap();
        }

        let config = MainConfig {
            listen: "127.0.0.1:0".to_string(),
            certificate,
            private_key,
            edge_ca_certificate,
            client_ca_certificate,
            edge_path: EDGE_PATH.to_string(),
            dbx_path: DBX_PATH.to_string(),
            max_connections: 1024,
            tls_handshake_timeout_secs: 10,
            http_header_timeout_secs: 10,
        };
        Self { _dir: dir, config, server_ca: server_ca.certificate, server_ca_certificate, edge_ca, client_ca }
    }

    async fn start(&self) -> MainGateway {
        MainGateway::bind(self.config.clone()).await.unwrap()
    }

    fn edge_config(&self, address: SocketAddr, edge_id: &str) -> EdgeConfig {
        let identity = issue_client(
            &self.edge_ca,
            &[&format!("urn:dbx-gateway:edge:{edge_id}")],
            ExtendedKeyUsagePurpose::ClientAuth,
            valid_window(),
        );
        let certificate = self._dir.0.join(format!("{edge_id}.pem"));
        let private_key = self._dir.0.join(format!("{edge_id}.key"));
        fs::write(&certificate, identity.certificate_pem).unwrap();
        fs::write(&private_key, identity.private_key_pem).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&private_key, fs::Permissions::from_mode(0o600)).unwrap();
        }
        EdgeConfig {
            edge_id: edge_id.to_string(),
            main_url: format!("wss://localhost:{}{EDGE_PATH}", address.port()),
            certificate,
            private_key,
            ca_certificate: self.server_ca_certificate.clone(),
            targets: BTreeMap::from([
                (
                    "postgres".to_string(),
                    EdgeTarget {
                        display_name: "PostgreSQL".to_string(),
                        address: TargetAddress::Tcp { tcp: "127.0.0.1:5432".to_string() },
                        allow_remote: false,
                    },
                ),
                (
                    "redis".to_string(),
                    EdgeTarget {
                        display_name: "Redis".to_string(),
                        address: TargetAddress::Tcp { tcp: "127.0.0.1:6379".to_string() },
                        allow_remote: false,
                    },
                ),
            ]),
        }
    }
}

#[tokio::test(start_paused = true)]
async fn control_edge_registers_only_logical_targets_and_refreshes_heartbeat() {
    let fixture = Fixture::new();
    let gateway = fixture.start().await;
    let edge = EdgeGateway::start(fixture.edge_config(gateway.local_addr(), "edge-prod-01")).unwrap();

    wait_for_edge(&gateway, "edge-prod-01", true).await;
    let before = gateway.registry().read().await["edge-prod-01"].last_heartbeat;
    advance(std::time::Duration::from_secs(16)).await;
    wait_for_heartbeat_after(&gateway, "edge-prod-01", before).await;

    let registry = gateway.registry();
    let entries = registry.read().await;
    let entry = &entries["edge-prod-01"];
    assert_eq!(entry.targets.keys().cloned().collect::<Vec<_>>(), ["postgres", "redis"]);
    assert_eq!(entry.targets["postgres"].display_name, "PostgreSQL");
    drop(entries);
    edge.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn control_rejects_registration_id_that_differs_from_certificate() {
    let fixture = Fixture::new();
    let gateway = fixture.start().await;
    let mut config = fixture.edge_config(gateway.local_addr(), "edge-cert-id");
    config.edge_id = "edge-claimed-id".to_string();
    let edge = EdgeGateway::start(config).unwrap();

    advance(std::time::Duration::from_secs(2)).await;
    tokio::task::yield_now().await;
    assert!(!gateway.registry().read().await.contains_key("edge-claimed-id"));
    assert!(!gateway.registry().read().await.contains_key("edge-cert-id"));
    edge.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn control_rejects_duplicate_online_edge_id() {
    let fixture = Fixture::new();
    let gateway = fixture.start().await;
    let first = EdgeGateway::start(fixture.edge_config(gateway.local_addr(), "edge-prod-01")).unwrap();
    wait_for_edge(&gateway, "edge-prod-01", true).await;
    let connection_id = gateway.registry().read().await["edge-prod-01"].connection_id;
    let second = EdgeGateway::start(fixture.edge_config(gateway.local_addr(), "edge-prod-01")).unwrap();

    advance(std::time::Duration::from_secs(2)).await;
    tokio::task::yield_now().await;
    assert_eq!(gateway.registry().read().await["edge-prod-01"].connection_id, connection_id);
    first.shutdown().await;
    second.shutdown().await;
}

#[tokio::test]
async fn control_connection_holds_a_main_connection_slot_until_disconnect() {
    let fixture = Fixture::new();
    let mut main_config = fixture.config.clone();
    main_config.max_connections = 1;
    let gateway = MainGateway::bind(main_config).await.unwrap();
    let edge_config = fixture.edge_config(gateway.local_addr(), "edge-capacity");
    let mut socket = connect_control_socket(&edge_config).await;
    send_registration(&mut socket, &edge_config).await;
    wait_for_edge(&gateway, "edge-capacity", true).await;

    assert!(request(gateway.local_addr(), &fixture.server_ca, None, EDGE_PATH).await.is_err());

    socket.close(None).await.unwrap();
    wait_for_edge(&gateway, "edge-capacity", false).await;
    assert_eq!(request(gateway.local_addr(), &fixture.server_ca, None, EDGE_PATH).await.unwrap(), 404);
}

#[tokio::test]
async fn control_edge_shutdown_cancels_an_incomplete_tls_connection() {
    let fixture = Fixture::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let edge = EdgeGateway::start(fixture.edge_config(listener.local_addr().unwrap(), "edge-stalled")).unwrap();
    let (_stream, _) = timeout(std::time::Duration::from_secs(2), listener.accept()).await.unwrap().unwrap();

    timeout(std::time::Duration::from_secs(1), edge.shutdown()).await.unwrap();
}

#[tokio::test]
async fn control_main_shutdown_cancels_a_socket_waiting_for_registration() {
    let fixture = Fixture::new();
    let gateway = fixture.start().await;
    let config = fixture.edge_config(gateway.local_addr(), "edge-no-registration");
    let _socket = connect_control_socket(&config).await;

    timeout(std::time::Duration::from_secs(2), gateway.shutdown()).await.unwrap();
}

#[tokio::test]
async fn control_main_shutdown_is_not_blocked_by_a_registry_reader() {
    let fixture = Fixture::new();
    let gateway = fixture.start().await;
    let edge = EdgeGateway::start(fixture.edge_config(gateway.local_addr(), "edge-read-lock")).unwrap();
    wait_for_edge(&gateway, "edge-read-lock", true).await;
    let registry = gateway.registry();
    let _reader = registry.read().await;

    timeout(std::time::Duration::from_secs(2), gateway.shutdown()).await.unwrap();
    edge.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn control_marks_silent_edge_offline_after_45_seconds() {
    let fixture = Fixture::new();
    let gateway = fixture.start().await;
    let config = fixture.edge_config(gateway.local_addr(), "edge-silent");
    let mut socket = connect_control_socket(&config).await;
    send_registration(&mut socket, &config).await;
    wait_for_edge(&gateway, "edge-silent", true).await;

    advance(std::time::Duration::from_secs(46)).await;
    wait_for_edge(&gateway, "edge-silent", false).await;
    assert!(socket.next().await.is_some());
}

#[tokio::test(start_paused = true)]
async fn control_disconnect_marks_edge_offline_immediately() {
    let fixture = Fixture::new();
    let gateway = fixture.start().await;
    let config = fixture.edge_config(gateway.local_addr(), "edge-disconnect");
    let mut socket = connect_control_socket(&config).await;
    send_registration(&mut socket, &config).await;
    wait_for_edge(&gateway, "edge-disconnect", true).await;

    socket.close(None).await.unwrap();
    wait_for_edge(&gateway, "edge-disconnect", false).await;
}

#[tokio::test(start_paused = true)]
async fn control_edge_reconnects_after_main_restart() {
    let fixture = Fixture::new();
    let first = fixture.start().await;
    let address = first.local_addr();
    let edge = EdgeGateway::start(fixture.edge_config(address, "edge-restart")).unwrap();
    wait_for_edge(&first, "edge-restart", true).await;

    first.shutdown().await;
    let mut config = fixture.config.clone();
    config.listen = address.to_string();
    let restarted = MainGateway::bind(config).await.unwrap();

    advance(std::time::Duration::from_secs(2)).await;
    wait_for_edge(&restarted, "edge-restart", true).await;

    edge.shutdown().await;
}

#[test]
fn control_reconnect_backoff_is_capped_jittered_and_resettable() {
    let mut backoff = ReconnectBackoff::new();
    let delays = (0..8).map(|_| backoff.next_delay(0)).collect::<Vec<_>>();
    assert_eq!(delays, [1, 2, 4, 8, 16, 32, 60, 60].map(std::time::Duration::from_secs));
    assert_eq!(backoff.next_delay(20), std::time::Duration::from_secs(72));
    backoff.reset();
    assert_eq!(backoff.next_delay(0), std::time::Duration::from_secs(1));
}

#[tokio::test]
async fn tls_without_certificate_can_handshake_but_all_paths_are_404() {
    let fixture = Fixture::new();
    let gateway = fixture.start().await;

    assert_eq!(request(gateway.local_addr(), &fixture.server_ca, None, EDGE_PATH).await.unwrap(), 404);
    assert_eq!(request(gateway.local_addr(), &fixture.server_ca, None, "/health").await.unwrap(), 404);
}

#[tokio::test]
async fn tls_anonymous_404_has_an_empty_body() {
    let fixture = Fixture::new();
    let gateway = fixture.start().await;

    let response = raw_request(gateway.local_addr(), &fixture.server_ca, None, EDGE_PATH).await.unwrap();
    let (_, body) = response.split_once("\r\n\r\n").unwrap();
    assert!(response.starts_with("HTTP/1.1 404"));
    assert!(body.is_empty());
}

#[tokio::test]
async fn tls_rejects_tls12_clients() {
    let fixture = Fixture::new();
    let gateway = fixture.start().await;

    assert!(connect_with_versions(gateway.local_addr(), &fixture.server_ca, None, &[&rustls::version::TLS12],)
        .await
        .is_err());
}

#[tokio::test]
async fn tls_connection_limit_and_handshake_timeout_release_capacity() {
    let fixture = Fixture::new();
    let mut config = fixture.config.clone();
    config.max_connections = 1;
    config.tls_handshake_timeout_secs = 1;
    let gateway = MainGateway::bind(config).await.unwrap();
    let _blocked = TcpStream::connect(gateway.local_addr()).await.unwrap();
    sleep(std::time::Duration::from_millis(50)).await;

    assert!(request(gateway.local_addr(), &fixture.server_ca, None, EDGE_PATH).await.is_err());
    sleep(std::time::Duration::from_secs(1)).await;
    assert_eq!(request(gateway.local_addr(), &fixture.server_ca, None, EDGE_PATH).await.unwrap(), 404);
}

#[tokio::test]
async fn tls_shutdown_closes_existing_connections() {
    let fixture = Fixture::new();
    let gateway = fixture.start().await;
    let mut stream = connect_with_versions(gateway.local_addr(), &fixture.server_ca, None, &[&rustls::version::TLS13])
        .await
        .unwrap();

    gateway.shutdown().await;
    let mut byte = [0_u8; 1];
    let result = timeout(std::time::Duration::from_secs(1), stream.read(&mut byte)).await.unwrap();
    assert!(matches!(result, Ok(0) | Err(_)));
}

#[tokio::test]
async fn tls_drop_closes_existing_connections() {
    let fixture = Fixture::new();
    let gateway = fixture.start().await;
    let mut stream = connect_with_versions(gateway.local_addr(), &fixture.server_ca, None, &[&rustls::version::TLS13])
        .await
        .unwrap();

    drop(gateway);
    let mut byte = [0_u8; 1];
    let result = timeout(std::time::Duration::from_secs(1), stream.read(&mut byte)).await.unwrap();
    assert!(matches!(result, Ok(0) | Err(_)));
}

#[tokio::test]
async fn tls_http_header_timeout_closes_idle_connection() {
    let fixture = Fixture::new();
    let mut config = fixture.config.clone();
    config.http_header_timeout_secs = 1;
    let gateway = MainGateway::bind(config).await.unwrap();
    let mut stream = connect_with_versions(gateway.local_addr(), &fixture.server_ca, None, &[&rustls::version::TLS13])
        .await
        .unwrap();

    let mut byte = [0_u8; 1];
    let result = timeout(std::time::Duration::from_secs(2), stream.read(&mut byte)).await.unwrap();
    assert!(matches!(result, Ok(0) | Err(_)));
}

#[tokio::test]
async fn tls_connection_closes_after_one_http_request() {
    let fixture = Fixture::new();
    let gateway = fixture.start().await;
    let mut stream = connect_with_versions(gateway.local_addr(), &fixture.server_ca, None, &[&rustls::version::TLS13])
        .await
        .unwrap();
    stream.write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n").await.unwrap();

    let mut response = Vec::new();
    timeout(std::time::Duration::from_secs(1), stream.read_to_end(&mut response)).await.unwrap().unwrap();
    assert!(String::from_utf8(response).unwrap().starts_with("HTTP/1.1 404"));
}

#[tokio::test]
async fn tls_loader_rejects_parent_directory_components() {
    let fixture = Fixture::new();
    let mut config = fixture.config.clone();
    config.certificate = config.certificate.parent().unwrap().join("..").join("certs/server.pem");

    assert!(MainGateway::bind(config).await.is_err());
}

#[tokio::test]
async fn tls_edge_identity_can_only_use_edge_path() {
    let fixture = Fixture::new();
    let identity = issue_client(
        &fixture.edge_ca,
        &["urn:dbx-gateway:edge:edge-prod-01"],
        ExtendedKeyUsagePurpose::ClientAuth,
        valid_window(),
    );
    let gateway = fixture.start().await;

    assert_eq!(request(gateway.local_addr(), &fixture.server_ca, Some(&identity), EDGE_PATH).await.unwrap(), 200);
    assert_eq!(request(gateway.local_addr(), &fixture.server_ca, Some(&identity), DBX_PATH).await.unwrap(), 404);
}

#[tokio::test]
async fn tls_dbx_client_identity_can_only_use_dbx_path() {
    let fixture = Fixture::new();
    let identity = issue_client(
        &fixture.client_ca,
        &["urn:dbx-gateway:client:desktop-01"],
        ExtendedKeyUsagePurpose::ClientAuth,
        valid_window(),
    );
    let gateway = fixture.start().await;

    assert_eq!(request(gateway.local_addr(), &fixture.server_ca, Some(&identity), DBX_PATH).await.unwrap(), 200);
    assert_eq!(request(gateway.local_addr(), &fixture.server_ca, Some(&identity), EDGE_PATH).await.unwrap(), 404);
}

#[tokio::test]
async fn tls_rejects_certificate_from_untrusted_ca() {
    let fixture = Fixture::new();
    let unknown_ca = make_ca("unknown-ca");
    let identity = issue_client(
        &unknown_ca,
        &["urn:dbx-gateway:edge:edge-prod-01"],
        ExtendedKeyUsagePurpose::ClientAuth,
        valid_window(),
    );
    let gateway = fixture.start().await;

    assert!(request(gateway.local_addr(), &fixture.server_ca, Some(&identity), EDGE_PATH).await.is_err());
}

#[tokio::test]
async fn tls_rejects_certificate_without_client_auth_eku() {
    let fixture = Fixture::new();
    let identity = issue_client(
        &fixture.edge_ca,
        &["urn:dbx-gateway:edge:edge-prod-01"],
        ExtendedKeyUsagePurpose::ServerAuth,
        valid_window(),
    );
    let gateway = fixture.start().await;

    assert!(request(gateway.local_addr(), &fixture.server_ca, Some(&identity), EDGE_PATH).await.is_err());
}

#[tokio::test]
async fn tls_rejects_certificate_without_any_eku() {
    let fixture = Fixture::new();
    let identity =
        issue_client_with_ekus(&fixture.edge_ca, &["urn:dbx-gateway:edge:edge-prod-01"], &[], valid_window());
    let gateway = fixture.start().await;

    assert!(request(gateway.local_addr(), &fixture.server_ca, Some(&identity), EDGE_PATH).await.is_err());
}

#[tokio::test]
async fn tls_rejects_certificate_with_mixed_client_and_server_eku() {
    let fixture = Fixture::new();
    let identity = issue_client_with_ekus(
        &fixture.edge_ca,
        &["urn:dbx-gateway:edge:edge-prod-01"],
        &[ExtendedKeyUsagePurpose::ClientAuth, ExtendedKeyUsagePurpose::ServerAuth],
        valid_window(),
    );
    let gateway = fixture.start().await;

    assert!(request(gateway.local_addr(), &fixture.server_ca, Some(&identity), EDGE_PATH).await.is_err());
}

#[tokio::test]
async fn tls_rejects_expired_certificate() {
    let fixture = Fixture::new();
    let now = OffsetDateTime::now_utc();
    let identity = issue_client(
        &fixture.edge_ca,
        &["urn:dbx-gateway:edge:edge-prod-01"],
        ExtendedKeyUsagePurpose::ClientAuth,
        (now - Duration::days(2), now - Duration::days(1)),
    );
    let gateway = fixture.start().await;

    assert!(request(gateway.local_addr(), &fixture.server_ca, Some(&identity), EDGE_PATH).await.is_err());
}

#[tokio::test]
async fn tls_rejects_role_ca_with_wrong_uri_role() {
    let fixture = Fixture::new();
    let identity = issue_client(
        &fixture.edge_ca,
        &["urn:dbx-gateway:client:desktop-01"],
        ExtendedKeyUsagePurpose::ClientAuth,
        valid_window(),
    );
    let gateway = fixture.start().await;

    assert!(request(gateway.local_addr(), &fixture.server_ca, Some(&identity), DBX_PATH).await.is_err());
}

#[tokio::test]
async fn tls_rejects_client_ca_with_edge_uri_role() {
    let fixture = Fixture::new();
    let identity = issue_client(
        &fixture.client_ca,
        &["urn:dbx-gateway:edge:edge-prod-01"],
        ExtendedKeyUsagePurpose::ClientAuth,
        valid_window(),
    );
    let gateway = fixture.start().await;

    assert!(request(gateway.local_addr(), &fixture.server_ca, Some(&identity), EDGE_PATH).await.is_err());
}

#[tokio::test]
async fn tls_rejects_ambiguous_identity_uri_sans() {
    let fixture = Fixture::new();
    let identity = issue_client(
        &fixture.edge_ca,
        &["urn:dbx-gateway:edge:edge-a", "urn:dbx-gateway:edge:edge-b"],
        ExtendedKeyUsagePurpose::ClientAuth,
        valid_window(),
    );
    let gateway = fixture.start().await;

    assert!(request(gateway.local_addr(), &fixture.server_ca, Some(&identity), EDGE_PATH).await.is_err());
}

fn make_ca(name: &str) -> TestCa {
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::default();
    params.distinguished_name.push(DnType::CommonName, name);
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::DigitalSignature];
    let certificate = params.self_signed(&key).unwrap();
    TestCa {
        certificate: certificate.der().clone(),
        certificate_pem: certificate.pem(),
        issuer: Issuer::new(params, key),
    }
}

fn issue_server(issuer: &Issuer<'_, KeyPair>) -> (String, String) {
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::default();
    params.distinguished_name.push(DnType::CommonName, "localhost");
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.subject_alt_names = vec![SanType::DnsName("localhost".try_into().unwrap())];
    let certificate = params.signed_by(&key, issuer).unwrap();
    (certificate.pem(), key.serialize_pem())
}

fn issue_client(
    ca: &TestCa,
    uris: &[&str],
    eku: ExtendedKeyUsagePurpose,
    validity: (OffsetDateTime, OffsetDateTime),
) -> ClientIdentity {
    issue_client_with_ekus(ca, uris, &[eku], validity)
}

fn issue_client_with_ekus(
    ca: &TestCa,
    uris: &[&str],
    ekus: &[ExtendedKeyUsagePurpose],
    validity: (OffsetDateTime, OffsetDateTime),
) -> ClientIdentity {
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::default();
    params.distinguished_name.push(DnType::CommonName, "gateway-peer");
    params.extended_key_usages = ekus.to_vec();
    params.subject_alt_names = uris.iter().map(|uri| SanType::URI((*uri).try_into().unwrap())).collect();
    params.not_before = validity.0;
    params.not_after = validity.1;
    let certificate = params.signed_by(&key, &ca.issuer).unwrap();
    ClientIdentity {
        certificates: vec![certificate.der().clone(), ca.certificate.clone()],
        private_key: PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
        certificate_pem: format!("{}{}", certificate.pem(), ca.certificate_pem),
        private_key_pem: key.serialize_pem(),
    }
}

fn valid_window() -> (OffsetDateTime, OffsetDateTime) {
    let now = OffsetDateTime::now_utc();
    (now - Duration::minutes(5), now + Duration::days(1))
}

async fn request(
    address: SocketAddr,
    server_ca: &CertificateDer<'static>,
    identity: Option<&ClientIdentity>,
    path: &str,
) -> Result<u16, Box<dyn std::error::Error>> {
    let response = raw_request(address, server_ca, identity, path).await?;
    let status = response.split_whitespace().nth(1).ok_or("missing response status")?.parse()?;
    Ok(status)
}

async fn raw_request(
    address: SocketAddr,
    server_ca: &CertificateDer<'static>,
    identity: Option<&ClientIdentity>,
    path: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut stream = connect_with_versions(address, server_ca, identity, &[&rustls::version::TLS13]).await?;
    stream.write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").as_bytes()).await?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await?;
    Ok(String::from_utf8(response)?)
}

async fn connect_with_versions(
    address: SocketAddr,
    server_ca: &CertificateDer<'static>,
    identity: Option<&ClientIdentity>,
    versions: &[&'static rustls::SupportedProtocolVersion],
) -> Result<tokio_rustls::client::TlsStream<TcpStream>, Box<dyn std::error::Error>> {
    let mut roots = RootCertStore::empty();
    roots.add(server_ca.clone())?;
    let builder = ClientConfig::builder_with_protocol_versions(versions).with_root_certificates(roots);
    let config = match identity {
        Some(identity) => {
            builder.with_client_auth_cert(identity.certificates.clone(), identity.private_key.clone_key())?
        }
        None => builder.with_no_client_auth(),
    };
    let stream = TcpStream::connect(address).await?;
    Ok(TlsConnector::from(Arc::new(config)).connect(ServerName::try_from("localhost").unwrap(), stream).await?)
}

async fn connect_control_socket(config: &EdgeConfig) -> WebSocketStream<MaybeTlsStream<TcpStream>> {
    let certificates =
        rustls_pemfile::certs(&mut std::io::BufReader::new(fs::File::open(&config.certificate).unwrap()))
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
    let private_key =
        rustls_pemfile::private_key(&mut std::io::BufReader::new(fs::File::open(&config.private_key).unwrap()))
            .unwrap()
            .unwrap();
    let mut roots = RootCertStore::empty();
    for certificate in
        rustls_pemfile::certs(&mut std::io::BufReader::new(fs::File::open(&config.ca_certificate).unwrap()))
    {
        roots.add(certificate.unwrap()).unwrap();
    }
    let client = ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .with_root_certificates(roots)
        .with_client_auth_cert(certificates, private_key)
        .unwrap();
    connect_async_tls_with_config(&config.main_url, None, false, Some(Connector::Rustls(Arc::new(client))))
        .await
        .unwrap()
        .0
}

async fn send_registration(socket: &mut WebSocketStream<MaybeTlsStream<TcpStream>>, config: &EdgeConfig) {
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
    socket.send(Message::Binary(encode_control_frame(&registration).unwrap().into())).await.unwrap();
}

async fn wait_for_edge(gateway: &MainGateway, edge_id: &str, online: bool) {
    timeout(std::time::Duration::from_secs(5), async {
        loop {
            if gateway.registry().read().await.get(edge_id).is_some_and(|entry| entry.online == online) {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

async fn wait_for_heartbeat_after(gateway: &MainGateway, edge_id: &str, before: Instant) {
    timeout(std::time::Duration::from_secs(5), async {
        loop {
            if gateway.registry().read().await[edge_id].last_heartbeat > before {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
