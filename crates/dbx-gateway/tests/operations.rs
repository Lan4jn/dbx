#![cfg(all(feature = "server", unix))]

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use dbx_gateway::config::{EdgeConfig, MainConfig};
use dbx_gateway::edge_gateway::EdgeGateway;
use dbx_gateway::limits::{BufferBudget, ConnectionRateLimiter, IdentityConcurrency, SecurityEvent, TargetPolicy};
use dbx_gateway::main_gateway::MainGateway;
use dbx_gateway::protocol::RegisteredTarget;
use dbx_gateway::state::GatewayState;
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    SanType,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use x509_parser::pem::parse_x509_pem;
use x509_parser::prelude::parse_x509_certificate;

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "dbx-gateway-operations-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(fs::canonicalize(path).unwrap())
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct TestCa {
    pem: String,
    issuer: Issuer<'static, KeyPair>,
}

fn ca(name: &str) -> TestCa {
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::default();
    params.distinguished_name.push(DnType::CommonName, name);
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params.not_before = time::OffsetDateTime::now_utc() - time::Duration::minutes(5);
    params.not_after = time::OffsetDateTime::now_utc() + time::Duration::days(1);
    let certificate = params.self_signed(&key).unwrap();
    TestCa { pem: certificate.pem(), issuer: Issuer::new(params, key) }
}

fn identity(ca: &TestCa, name: &str, server: bool) -> (String, String) {
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::default();
    params.distinguished_name.push(DnType::CommonName, name);
    params.extended_key_usages =
        vec![if server { ExtendedKeyUsagePurpose::ServerAuth } else { ExtendedKeyUsagePurpose::ClientAuth }];
    params.not_before = time::OffsetDateTime::now_utc() - time::Duration::minutes(5);
    params.not_after = time::OffsetDateTime::now_utc() + time::Duration::hours(12);
    if server {
        params.subject_alt_names.push(SanType::DnsName("localhost".try_into().unwrap()));
    } else {
        params.subject_alt_names.push(SanType::URI("urn:dbx-gateway:edge:edge-ops".try_into().unwrap()));
    }
    let certificate = params.signed_by(&key, &ca.issuer).unwrap();
    (format!("{}{}", certificate.pem(), ca.pem), key.serialize_pem())
}

fn write_identity(dir: &TempDir, stem: &str, certificate: &str, key: &str) -> (PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let certificate_path = dir.0.join(format!("{stem}.pem"));
    let key_path = dir.0.join(format!("{stem}.key"));
    fs::write(&certificate_path, certificate).unwrap();
    fs::write(&key_path, key).unwrap();
    fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600)).unwrap();
    (certificate_path, key_path)
}

#[tokio::test]
async fn reload_preserves_valid_runtime_on_error_and_closes_revoked_edge() {
    let dir = TempDir::new();
    let server_ca = ca("server-ca");
    let edge_ca = ca("edge-ca");
    let client_ca = ca("client-ca");
    let (server_certificate, server_key) = identity(&server_ca, "localhost", true);
    let (edge_certificate, edge_key) = identity(&edge_ca, "edge-ops", false);
    let (server_certificate, server_key) = write_identity(&dir, "server", &server_certificate, &server_key);
    let (edge_certificate_path, edge_key) = write_identity(&dir, "edge", &edge_certificate, &edge_key);
    let edge_ca_path = dir.0.join("edge-ca.pem");
    let client_ca_path = dir.0.join("client-ca.pem");
    let server_ca_path = dir.0.join("server-ca.pem");
    fs::write(&edge_ca_path, &edge_ca.pem).unwrap();
    fs::write(&client_ca_path, &client_ca.pem).unwrap();
    fs::write(&server_ca_path, &server_ca.pem).unwrap();
    let config = MainConfig {
        listen: "127.0.0.1:0".to_string(),
        certificate: server_certificate,
        private_key: server_key,
        edge_ca_certificate: edge_ca_path,
        client_ca_certificate: client_ca_path,
        edge_path: "/_dbx/edge".to_string(),
        dbx_path: "/_dbx/client".to_string(),
        max_connections: 64,
        tls_handshake_timeout_secs: 5,
        http_header_timeout_secs: 5,
        enrollment: None,
        allowed_edge_ids: vec!["edge-ops".to_string()],
        revoked_edge_serials: Vec::new(),
        client_route_acl: BTreeMap::new(),
        fallback_upstream: None,
        health_listen: Some("127.0.0.1:0".to_string()),
        state_file: Some(dir.0.join("main-state.sqlite3")),
        max_streams_per_edge: 1,
        max_streams_per_client: 32,
        connection_rate_per_second: 64,
        connection_rate_burst: 128,
        global_buffer_budget_bytes: 256 * 1024 * 1024,
    };
    let main = MainGateway::bind(config.clone()).await.unwrap();
    let edge = EdgeGateway::start(EdgeConfig {
        edge_id: "edge-ops".to_string(),
        main_url: format!("wss://localhost:{}/_dbx/edge", main.local_addr().port()),
        certificate: edge_certificate_path,
        private_key: edge_key,
        ca_certificate: server_ca_path,
        targets: BTreeMap::new(),
        bootstrap: None,
    })
    .unwrap();
    wait_online(&main, true).await;

    let mut health = TcpStream::connect(main.health_addr().unwrap()).await.unwrap();
    health.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").await.unwrap();
    let mut health_response = String::new();
    health.read_to_string(&mut health_response).await.unwrap();
    assert!(health_response.starts_with("HTTP/1.1 200 OK"));
    assert!(health_response.contains("\"online_edges\":1"));
    assert!(health_response.contains("\"database_checks\":0"));
    assert!(health_response.contains("\"process_id\":"));
    assert!(health_response.contains("\"server_certificate_not_after_unix\":"));
    assert!(health_response.contains("\"pki_configured\":false"));

    let mut invalid = config.clone();
    invalid.certificate = dir.0.join("missing.pem");
    assert!(main.reload(invalid).await.is_err());
    assert!(main.registry().read().await["edge-ops"].online);

    let (_, pem) = parse_x509_pem(edge_certificate.as_bytes()).unwrap();
    let (_, certificate) = parse_x509_certificate(&pem.contents).unwrap();
    let mut revoked = config;
    revoked.revoked_edge_serials = vec![certificate.raw_serial_as_string()];
    main.reload(revoked).await.unwrap();
    wait_online(&main, false).await;

    edge.shutdown().await;
    main.shutdown().await;
}

#[tokio::test]
async fn limits_target_policy_rejects_unsafe_addresses() {
    assert!(TargetPolicy::new(false).resolve_and_validate("127.0.0.1:5432").await.is_ok());
    assert!(TargetPolicy::new(false).resolve_and_validate("0.0.0.0:5432").await.is_err());
    assert!(TargetPolicy::new(true).resolve_and_validate("169.254.169.254:80").await.is_err());
    assert!(TargetPolicy::new(true).resolve_and_validate("[fe80::1]:80").await.is_err());
}

#[test]
fn limits_security_events_never_serialize_payload_secrets() {
    let event = SecurityEvent {
        request_id: Some("request-1"),
        peer_role: Some("dbx_client"),
        peer_id: Some("desktop-1"),
        edge_id: Some("edge-ops"),
        target_id: Some("postgres"),
        stage: Some("stream_ready"),
        ..SecurityEvent::default()
    };
    let encoded = event.to_json();
    for secret in ["SELECT secret_marker", "token-secret-marker", "BEGIN PRIVATE KEY"] {
        assert!(!encoded.contains(secret));
    }
}

#[test]
fn limits_rate_concurrency_and_buffer_budget_fail_closed() {
    let rate = ConnectionRateLimiter::new(1, 2);
    let now = Instant::now();
    assert!(rate.allow("127.0.0.1", now));
    assert!(rate.allow("127.0.0.1", now));
    assert!(!rate.allow("127.0.0.1", now));
    assert!(rate.allow("127.0.0.1", now + Duration::from_secs(1)));

    let concurrency = IdentityConcurrency::new(1);
    let first = concurrency.try_acquire("desktop-1").unwrap();
    assert!(concurrency.try_acquire("desktop-1").is_none());
    drop(first);
    assert!(concurrency.try_acquire("desktop-1").is_some());

    let budget = BufferBudget::new(1024);
    let reservation = budget.try_reserve(768).unwrap();
    assert!(budget.try_reserve(512).is_none());
    drop(reservation);
    assert!(budget.try_reserve(1024).is_some());
}

#[tokio::test]
async fn persisted_routes_contain_only_offline_logical_metadata() {
    let dir = TempDir::new();
    let state_path = dir.0.join("main-state.sqlite3");
    let state = GatewayState::open(state_path.clone()).await.unwrap();
    let targets = BTreeMap::from([(
        "postgres-primary".to_string(),
        RegisteredTarget { target_id: "postgres-primary".to_string(), display_name: "Primary database".to_string() },
    )]);

    state.replace_edge_routes("edge-prod-01", &targets).await.unwrap();
    let restored = state.load_edge_routes().await.unwrap();

    assert_eq!(restored["edge-prod-01"], targets);
    let database = fs::read(state_path).unwrap();
    assert!(!database.windows(b"10.20.30.40:5432".len()).any(|window| window == b"10.20.30.40:5432"));
}

async fn wait_online(main: &MainGateway, online: bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if main.registry().read().await.get("edge-ops").is_some_and(|entry| entry.online == online) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
}
