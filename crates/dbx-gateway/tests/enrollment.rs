#![cfg(feature = "server")]

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use dbx_gateway::config::{
    EdgeBootstrapConfig, EdgeConfig, EdgeTarget, MainConfig, MainEnrollmentConfig, PkiEndpointConfig, TargetAddress,
};
use dbx_gateway::edge_gateway::EdgeGateway;
use dbx_gateway::main_gateway::MainGateway;
use dbx_gateway::pki::{
    enroll_over_remote, enroll_over_unix, renew_over_unix, serve_remote, serve_unix, EnrollCsrRequest,
    PkiEnrollmentService, PkiStore, RemotePkiConfig, RenewCsrRequest,
};
use dbx_gateway::state::GatewayState;
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    SanType,
};
use sha2::Digest;
use x509_parser::extensions::GeneralName;
use x509_parser::pem::parse_x509_pem;
use x509_parser::prelude::parse_x509_certificate;
use zeroize::Zeroizing;

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        loop {
            let id = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("dbx-gateway-enrollment-{}-{id}", std::process::id()));
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

async fn test_state() -> (TempDir, GatewayState) {
    let dir = TempDir::new();
    let state = GatewayState::open(dir.0.join("state.sqlite3")).await.unwrap();
    (dir, state)
}

#[tokio::test]
async fn token_database_stores_only_an_argon2id_hash() {
    let (_dir, state) = test_state().await;
    let token = state.enrollments.create("edge-prod-01", Duration::from_secs(600), false).await.unwrap();
    let bytes = fs::read(state.path()).unwrap();

    assert!(!bytes.windows(token.secret.len()).any(|window| window == token.secret.as_bytes()));
    assert!(bytes.windows(10).any(|window| window == b"$argon2id$"));
    assert!(token.expires_at - token.created_at >= time::Duration::minutes(9));
}

#[tokio::test]
async fn token_is_edge_bound_and_consumed_once_under_concurrency() {
    let (_dir, state) = test_state().await;
    let token = state.enrollments.create("edge-prod-01", Duration::from_secs(600), false).await.unwrap();
    assert!(state.enrollments.consume("edge-prod-02", &token.secret).await.is_err());

    let results = futures_util::future::join_all((0..20).map(|_| {
        let enrollments = state.enrollments.clone();
        let secret = token.secret.to_string();
        async move { enrollments.consume("edge-prod-01", &secret).await }
    }))
    .await;

    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
}

#[tokio::test]
async fn token_revocation_prevents_consumption() {
    let (_dir, state) = test_state().await;
    let token = state.enrollments.create("edge-prod-01", Duration::from_secs(600), false).await.unwrap();
    state.enrollments.revoke(token.id).await.unwrap();

    assert!(state.enrollments.consume("edge-prod-01", &token.secret).await.is_err());
}

#[tokio::test]
async fn token_expiration_prevents_consumption() {
    let (_dir, state) = test_state().await;
    let token = state.enrollments.create("edge-prod-01", Duration::from_secs(1), false).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;

    assert!(state.enrollments.consume("edge-prod-01", &token.secret).await.is_err());
}

#[tokio::test]
async fn token_replace_is_required_for_an_edge_with_an_active_certificate() {
    let (_dir, state) = test_state().await;
    state.record_issued_certificate("edge-prod-01", "01AB").await.unwrap();

    assert!(state.enrollments.create("edge-prod-01", Duration::from_secs(600), false).await.is_err());
    let token = state.enrollments.create("edge-prod-01", Duration::from_secs(600), true).await.unwrap();

    assert!(token.replace);
    assert!(state.certificate_is_revoked("01AB").await.unwrap());
}

#[cfg(unix)]
#[tokio::test]
async fn pki_service_and_renewal_issue_only_the_authenticated_edge_identity() {
    use std::os::unix::fs::PermissionsExt;

    let (dir, state) = test_state().await;
    let password = Zeroizing::new("test-ca-password".to_string());
    let store = PkiStore::init(&dir.0.join("pki"), &password).unwrap();
    let service = PkiEnrollmentService::new(state.clone(), store, password);
    let socket_path = dir.0.join("pki.sock");
    let server =
        serve_unix(socket_path.clone(), unsafe { libc::geteuid() }, unsafe { libc::getegid() }, service.clone())
            .await
            .unwrap();
    assert_eq!(fs::metadata(&socket_path).unwrap().permissions().mode() & 0o777, 0o660);

    let token = state.enrollments.create("edge-prod-01", Duration::from_secs(600), false).await.unwrap();
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::default();
    params.distinguished_name.push(DnType::CommonName, "attacker-controlled");
    params.subject_alt_names.push(SanType::URI("urn:dbx-gateway:client:attacker".try_into().unwrap()));
    let csr = params.serialize_request(&key).unwrap();
    let rejected = EnrollCsrRequest {
        token: Zeroizing::new(token.secret.to_string()),
        claimed_edge_id: "edge-prod-02".to_string(),
        csr_der: csr.der().to_vec(),
    };
    assert!(enroll_over_unix(&socket_path, &rejected).await.is_err());

    let response = enroll_over_unix(
        &socket_path,
        &EnrollCsrRequest {
            token: Zeroizing::new(token.secret.to_string()),
            claimed_edge_id: "edge-prod-01".to_string(),
            csr_der: csr.der().to_vec(),
        },
    )
    .await
    .unwrap();
    let (_, pem) = parse_x509_pem(response.certificate_pem.as_bytes()).unwrap();
    let (_, certificate) = parse_x509_certificate(&pem.contents).unwrap();
    let eku = certificate.extended_key_usage().unwrap().unwrap();
    assert!(eku.value.client_auth);
    assert!(!eku.value.server_auth);
    let sans = certificate.subject_alternative_name().unwrap().unwrap();
    let uris = sans
        .value
        .general_names
        .iter()
        .filter_map(|name| match name {
            GeneralName::URI(uri) => Some(*uri),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(uris, ["urn:dbx-gateway:edge:edge-prod-01"]);
    assert_eq!(certificate.public_key().subject_public_key.data.as_ref(), key.public_key_raw());

    let renewal_key = KeyPair::generate().unwrap();
    let renewal_csr = CertificateParams::default().serialize_request(&renewal_key).unwrap();
    let renewed = renew_over_unix(
        &socket_path,
        &RenewCsrRequest {
            edge_id: "edge-prod-01".to_string(),
            current_serial: response.serial_hex.clone(),
            csr_der: renewal_csr.der().to_vec(),
        },
    )
    .await
    .unwrap();
    assert_ne!(renewed.serial_hex, response.serial_hex);
    assert!(state.certificate_is_revoked(&response.serial_hex).await.unwrap());
    assert!(renew_over_unix(
        &socket_path,
        &RenewCsrRequest {
            edge_id: "edge-prod-01".to_string(),
            current_serial: response.serial_hex.clone(),
            csr_der: renewal_csr.der().to_vec(),
        },
    )
    .await
    .is_err());

    let failed_token = state.enrollments.create("edge-failed-csr", Duration::from_secs(600), false).await.unwrap();
    let failed = EnrollCsrRequest {
        token: Zeroizing::new(failed_token.secret.to_string()),
        claimed_edge_id: "edge-failed-csr".to_string(),
        csr_der: vec![1, 2, 3],
    };
    assert!(enroll_over_unix(&socket_path, &failed).await.is_err());
    let retry_key = KeyPair::generate().unwrap();
    let retry_csr = CertificateParams::default().serialize_request(&retry_key).unwrap();
    let retry = EnrollCsrRequest {
        token: Zeroizing::new(failed_token.secret.to_string()),
        claimed_edge_id: "edge-failed-csr".to_string(),
        csr_der: retry_csr.der().to_vec(),
    };
    assert!(enroll_over_unix(&socket_path, &retry).await.is_err());

    server.shutdown().await;

    let server_ca = test_ca("remote-server-ca");
    let ra_ca = test_ca("main-ra-ca");
    let server_identity = issue_test_identity(&server_ca, "localhost", None, true);
    let allowed_ra = issue_test_identity(&ra_ca, "main-ra", Some("urn:dbx-gateway:ra:main-01"), false);
    let denied_ra = issue_test_identity(&ra_ca, "other-ra", Some("urn:dbx-gateway:ra:other"), false);
    let server_ca_path = dir.0.join("remote-server-ca.pem");
    let ra_ca_path = dir.0.join("main-ra-ca.pem");
    let server_cert = dir.0.join("remote-server.pem");
    let server_key = dir.0.join("remote-server.key");
    let allowed_cert = dir.0.join("main-ra.pem");
    let allowed_key = dir.0.join("main-ra.key");
    let denied_cert = dir.0.join("other-ra.pem");
    let denied_key = dir.0.join("other-ra.key");
    fs::write(&server_ca_path, &server_ca.certificate_pem).unwrap();
    fs::write(&ra_ca_path, &ra_ca.certificate_pem).unwrap();
    write_test_identity(&server_cert, &server_key, &server_identity, &server_ca.certificate_pem);
    write_test_identity(&allowed_cert, &allowed_key, &allowed_ra, &ra_ca.certificate_pem);
    write_test_identity(&denied_cert, &denied_key, &denied_ra, &ra_ca.certificate_pem);
    let remote = serve_remote(
        RemotePkiConfig {
            listen: "127.0.0.1:0".to_string(),
            certificate: server_cert,
            private_key: server_key,
            main_ra_ca_certificate: ra_ca_path,
            allowed_ra_uri_sans: vec!["urn:dbx-gateway:ra:main-01".to_string()],
        },
        service,
    )
    .await
    .unwrap();
    let remote_token = state.enrollments.create("edge-remote-01", Duration::from_secs(600), false).await.unwrap();
    let remote_key = KeyPair::generate().unwrap();
    let remote_csr = CertificateParams::default().serialize_request(&remote_key).unwrap();
    let remote_request = EnrollCsrRequest {
        token: Zeroizing::new(remote_token.secret.to_string()),
        claimed_edge_id: "edge-remote-01".to_string(),
        csr_der: remote_csr.der().to_vec(),
    };
    assert!(enroll_over_remote(
        remote.local_addr(),
        "localhost",
        &server_ca_path,
        &denied_cert,
        &denied_key,
        &remote_request,
    )
    .await
    .is_err());
    let remote_response = enroll_over_remote(
        remote.local_addr(),
        "localhost",
        &server_ca_path,
        &allowed_cert,
        &allowed_key,
        &remote_request,
    )
    .await
    .unwrap();
    assert_eq!(remote_response.edge_id, "edge-remote-01");
    remote.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn bootstrap_edge_generates_and_installs_its_own_identity_through_main() {
    use std::os::unix::fs::PermissionsExt;

    let (dir, state) = test_state().await;
    let password = Zeroizing::new("bootstrap-ca-password".to_string());
    let store = PkiStore::init(&dir.0.join("pki"), &password).unwrap();
    let edge_ca_certificate = store.edge_ca_certificate_path();
    let service = PkiEnrollmentService::new(state.clone(), store, password);
    let pki_socket = dir.0.join("b.sock");
    let pki_server =
        serve_unix(pki_socket.clone(), unsafe { libc::geteuid() }, unsafe { libc::getegid() }, service).await.unwrap();

    let server_ca = test_ca("bootstrap-server-ca");
    let server_identity = issue_test_identity(&server_ca, "localhost", None, true);
    let server_ca_path = dir.0.join("bootstrap-server-ca.pem");
    let server_certificate = dir.0.join("bootstrap-server.pem");
    let server_key = dir.0.join("bootstrap-server.key");
    fs::write(&server_ca_path, &server_ca.certificate_pem).unwrap();
    write_test_identity(&server_certificate, &server_key, &server_identity, &server_ca.certificate_pem);
    let (_, server_pem) = parse_x509_pem(server_identity.certificate_pem.as_bytes()).unwrap();
    let (_, server_x509) = parse_x509_certificate(&server_pem.contents).unwrap();
    let server_pin = hex::encode(sha2::Sha256::digest(server_x509.public_key().raw));
    let main = MainGateway::bind(MainConfig {
        listen: "127.0.0.1:0".to_string(),
        certificate: server_certificate,
        private_key: server_key,
        edge_ca_certificate,
        client_ca_certificate: server_ca_path.clone(),
        edge_path: "/_dbx/edge".to_string(),
        dbx_path: "/_dbx/client".to_string(),
        max_connections: 64,
        tls_handshake_timeout_secs: 5,
        http_header_timeout_secs: 5,
        enrollment: Some(MainEnrollmentConfig {
            path: "/_dbx/enroll".to_string(),
            renewal_path: "/_dbx/renew".to_string(),
            allowed_edge_ids: vec!["edge-bootstrap-01".to_string()],
            pki: PkiEndpointConfig::Unix { unix_socket: pki_socket },
        }),
        allowed_edge_ids: Vec::new(),
        revoked_edge_serials: Vec::new(),
        client_route_acl: BTreeMap::new(),
        fallback_upstream: None,
        health_listen: None,
        state_file: None,
        max_streams_per_edge: 256,
        max_streams_per_client: 32,
        connection_rate_per_second: 64,
        connection_rate_burst: 128,
        global_buffer_budget_bytes: 256 * 1024 * 1024,
    })
    .await
    .unwrap();

    let token = state.enrollments.create("edge-bootstrap-01", Duration::from_secs(600), false).await.unwrap();
    let token_file = dir.0.join("edge-bootstrap.token");
    fs::write(&token_file, token.secret.as_bytes()).unwrap();
    fs::set_permissions(&token_file, fs::Permissions::from_mode(0o600)).unwrap();
    let certificate = dir.0.join("edge-bootstrap.pem");
    let private_key = dir.0.join("edge-bootstrap.key");
    let edge = EdgeGateway::start(EdgeConfig {
        edge_id: "edge-bootstrap-01".to_string(),
        main_url: format!("wss://localhost:{}/_dbx/edge", main.local_addr().port()),
        certificate: certificate.clone(),
        private_key: private_key.clone(),
        ca_certificate: server_ca_path,
        targets: BTreeMap::from([(
            "postgres".to_string(),
            EdgeTarget {
                display_name: "PostgreSQL".to_string(),
                address: TargetAddress::Tcp { tcp: "127.0.0.1:5432".to_string() },
                allow_remote: false,
            },
        )]),
        bootstrap: Some(EdgeBootstrapConfig {
            token_file: token_file.clone(),
            enrollment_url: format!("https://localhost:{}/_dbx/enroll", main.local_addr().port()),
            server_spki_sha256: server_pin.clone(),
            renew_before_days: 120,
        }),
    })
    .unwrap();

    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if main.registry().read().await.get("edge-bootstrap-01").is_some_and(|entry| entry.online) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert!(certificate.is_file());
    assert!(private_key.is_file());
    assert_eq!(fs::metadata(&private_key).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(!token_file.exists());
    assert_eq!(state.revocation_count_for_edge("edge-bootstrap-01").await.unwrap(), 1);
    assert!(!walk_file_names(&dir.0.join("pki")).iter().any(|name| name.contains("edge-bootstrap.key")));

    let denied_token = state.enrollments.create("edge-not-allowed", Duration::from_secs(600), false).await.unwrap();
    let denied_token_file = dir.0.join("edge-denied.token");
    fs::write(&denied_token_file, denied_token.secret.as_bytes()).unwrap();
    fs::set_permissions(&denied_token_file, fs::Permissions::from_mode(0o600)).unwrap();
    let denied_certificate = dir.0.join("edge-denied.pem");
    let denied_private_key = dir.0.join("edge-denied.key");
    let denied = EdgeGateway::start(EdgeConfig {
        edge_id: "edge-not-allowed".to_string(),
        main_url: format!("wss://localhost:{}/_dbx/edge", main.local_addr().port()),
        certificate: denied_certificate.clone(),
        private_key: denied_private_key.clone(),
        ca_certificate: dir.0.join("bootstrap-server-ca.pem"),
        targets: BTreeMap::new(),
        bootstrap: Some(EdgeBootstrapConfig {
            token_file: denied_token_file.clone(),
            enrollment_url: format!("https://localhost:{}/_dbx/enroll", main.local_addr().port()),
            server_spki_sha256: server_pin,
            renew_before_days: 30,
        }),
    })
    .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(denied_token_file.exists());
    assert!(!denied_certificate.exists());
    assert!(!denied_private_key.exists());

    denied.shutdown().await;
    edge.shutdown().await;
    main.shutdown().await;
    pki_server.shutdown().await;
}

fn walk_file_names(path: &std::path::Path) -> Vec<String> {
    let mut names = Vec::new();
    let mut pending = vec![path.to_path_buf()];
    while let Some(path) = pending.pop() {
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    pending.push(path);
                } else if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
                    names.push(name.to_string());
                }
            }
        }
    }
    names
}

struct TestCa {
    certificate_pem: String,
    issuer: Issuer<'static, KeyPair>,
}

struct TestIdentity {
    certificate_pem: String,
    private_key_pem: String,
}

fn test_ca(name: &str) -> TestCa {
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::default();
    params.distinguished_name.push(DnType::CommonName, name);
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params.not_before = time::OffsetDateTime::now_utc() - time::Duration::minutes(5);
    params.not_after = time::OffsetDateTime::now_utc() + time::Duration::days(1);
    let certificate = params.self_signed(&key).unwrap();
    TestCa { certificate_pem: certificate.pem(), issuer: Issuer::new(params, key) }
}

fn issue_test_identity(ca: &TestCa, name: &str, uri: Option<&str>, server: bool) -> TestIdentity {
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::default();
    params.distinguished_name.push(DnType::CommonName, name);
    params.extended_key_usages =
        vec![if server { ExtendedKeyUsagePurpose::ServerAuth } else { ExtendedKeyUsagePurpose::ClientAuth }];
    params.not_before = time::OffsetDateTime::now_utc() - time::Duration::minutes(5);
    params.not_after = time::OffsetDateTime::now_utc() + time::Duration::hours(12);
    if server {
        params.subject_alt_names.push(SanType::DnsName("localhost".try_into().unwrap()));
    }
    if let Some(uri) = uri {
        params.subject_alt_names.push(SanType::URI(uri.try_into().unwrap()));
    }
    let certificate = params.signed_by(&key, &ca.issuer).unwrap();
    TestIdentity { certificate_pem: certificate.pem(), private_key_pem: key.serialize_pem() }
}

fn write_test_identity(certificate: &std::path::Path, key: &std::path::Path, identity: &TestIdentity, ca_pem: &str) {
    fs::write(certificate, format!("{}{ca_pem}", identity.certificate_pem)).unwrap();
    fs::write(key, &identity.private_key_pem).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(key, fs::Permissions::from_mode(0o600)).unwrap();
    }
}
