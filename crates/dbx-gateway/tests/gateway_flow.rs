#![cfg(feature = "server")]

use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use dbx_gateway::config::MainConfig;
use dbx_gateway::main_gateway::MainGateway;
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    SanType,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
use time::{Duration, OffsetDateTime};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout};
use tokio_rustls::TlsConnector;

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
}

struct Fixture {
    _dir: TempDir,
    config: MainConfig,
    server_ca: CertificateDer<'static>,
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
        fs::write(&certificate, server_certificate).unwrap();
        fs::write(&private_key, server_key).unwrap();
        fs::write(&edge_ca_certificate, &edge_ca.certificate_pem).unwrap();
        fs::write(&client_ca_certificate, &client_ca.certificate_pem).unwrap();
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
        Self { _dir: dir, config, server_ca: server_ca.certificate, edge_ca, client_ca }
    }

    async fn start(&self) -> MainGateway {
        MainGateway::bind(self.config.clone()).await.unwrap()
    }
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
