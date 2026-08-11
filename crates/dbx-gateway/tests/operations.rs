#![cfg(all(feature = "server", unix))]

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use dbx_gateway::config::{EdgeConfig, MainConfig};
use dbx_gateway::edge_gateway::EdgeGateway;
use dbx_gateway::main_gateway::MainGateway;
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    SanType,
};
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
        fallback_upstream: None,
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
