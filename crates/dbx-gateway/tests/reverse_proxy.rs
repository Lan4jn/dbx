#![cfg(all(feature = "server", unix))]

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dbx_gateway::config::MainConfig;
use dbx_gateway::main_gateway::MainGateway;
use futures_util::{SinkExt, StreamExt};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    SanType,
};
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsConnector;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async_tls_with_config, Connector, WebSocketStream};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "dbx-gateway-proxy-{}-{}",
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
    der: CertificateDer<'static>,
    pem: String,
    issuer: Issuer<'static, KeyPair>,
}

struct Fixture {
    _dir: TempDir,
    config: MainConfig,
    server_ca: CertificateDer<'static>,
}

impl Fixture {
    fn new(upstream: String) -> Self {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new();
        let server_ca = ca("server-ca");
        let edge_ca = ca("edge-ca");
        let client_ca = ca("client-ca");
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::default();
        params.distinguished_name.push(DnType::CommonName, "localhost");
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.subject_alt_names.push(SanType::DnsName("localhost".try_into().unwrap()));
        params.not_before = time::OffsetDateTime::now_utc() - time::Duration::minutes(5);
        params.not_after = time::OffsetDateTime::now_utc() + time::Duration::hours(12);
        let certificate = params.signed_by(&key, &server_ca.issuer).unwrap();
        let certificate_path = dir.0.join("server.pem");
        let key_path = dir.0.join("server.key");
        let edge_ca_path = dir.0.join("edge-ca.pem");
        let client_ca_path = dir.0.join("client-ca.pem");
        fs::write(&certificate_path, format!("{}{}", certificate.pem(), server_ca.pem)).unwrap();
        fs::write(&key_path, key.serialize_pem()).unwrap();
        fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&edge_ca_path, edge_ca.pem).unwrap();
        fs::write(&client_ca_path, client_ca.pem).unwrap();
        let config = MainConfig {
            listen: "127.0.0.1:0".to_string(),
            certificate: certificate_path,
            private_key: key_path,
            edge_ca_certificate: edge_ca_path,
            client_ca_certificate: client_ca_path,
            edge_path: "/_dbx/edge".to_string(),
            dbx_path: "/_dbx/client".to_string(),
            max_connections: 64,
            tls_handshake_timeout_secs: 5,
            http_header_timeout_secs: 5,
            enrollment: None,
            allowed_edge_ids: Vec::new(),
            revoked_edge_serials: Vec::new(),
            client_route_acl: BTreeMap::new(),
            fallback_upstream: Some(upstream),
            health_listen: None,
            state_file: None,
            max_streams_per_edge: 256,
            max_streams_per_client: 32,
            connection_rate_per_second: 64,
            connection_rate_burst: 128,
            global_buffer_budget_bytes: 256 * 1024 * 1024,
        };
        Self { _dir: dir, config, server_ca: server_ca.der }
    }
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
    TestCa { der: certificate.der().clone(), pem: certificate.pem(), issuer: Issuer::new(params, key) }
}

#[tokio::test]
async fn fallback_proxies_only_ordinary_paths_and_sets_forwarding_headers() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_address = upstream.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let count = Arc::new(AtomicUsize::new(0));
    let task = {
        let requests = requests.clone();
        let count = count.clone();
        tokio::spawn(async move {
            let (mut stream, _) = upstream.accept().await.unwrap();
            let request = read_headers(&mut stream).await;
            count.fetch_add(1, Ordering::SeqCst);
            requests.lock().unwrap().push(request);
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello").await.unwrap();
        })
    };
    let fixture = Fixture::new(format!("http://{upstream_address}/base"));
    let main = MainGateway::bind(fixture.config).await.unwrap();

    let response = request(main.local_addr(), &fixture.server_ca, "/hello?q=1").await;
    assert!(response.starts_with("HTTP/1.1 200"));
    assert!(response.ends_with("hello"));
    task.await.unwrap();
    let captured = requests.lock().unwrap()[0].to_ascii_lowercase();
    assert!(captured.starts_with("get /base/hello?q=1 http/1.1"));
    assert!(captured.contains(&format!("host: {upstream_address}").to_ascii_lowercase()));
    assert!(captured.contains("x-forwarded-for: 127.0.0.1"));
    assert!(captured.contains("x-forwarded-proto: https"));

    let reserved = request(main.local_addr(), &fixture.server_ca, "/_dbx/client").await;
    assert!(reserved.starts_with("HTTP/1.1 404"));
    assert_eq!(count.load(Ordering::SeqCst), 1);
    main.shutdown().await;
}

#[tokio::test]
async fn fallback_streams_sse_before_the_upstream_closes() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_address = upstream.local_addr().unwrap();
    let (sent, sent_rx) = tokio::sync::oneshot::channel();
    let (release, release_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let (mut stream, _) = upstream.accept().await.unwrap();
        let _ = read_headers(&mut stream).await;
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\nd\r\ndata: ready\n\n\r\n",
            )
            .await
            .unwrap();
        let _ = sent.send(());
        let _ = release_rx.await;
        stream.write_all(b"0\r\n\r\n").await.unwrap();
    });
    let fixture = Fixture::new(format!("http://{upstream_address}"));
    let main = MainGateway::bind(fixture.config).await.unwrap();
    let mut stream = tls_stream(main.local_addr(), &fixture.server_ca).await;
    stream.write_all(b"GET /events HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").await.unwrap();
    sent_rx.await.unwrap();
    let mut response = vec![0_u8; 4096];
    let count = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut response)).await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&response[..count]).contains("data: ready"));
    let _ = release.send(());
    task.await.unwrap();
    main.shutdown().await;
}

#[tokio::test]
async fn fallback_relays_websocket_text_and_binary_frames() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_address = upstream.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut stream, _) = upstream.accept().await.unwrap();
        let headers = read_headers(&mut stream).await;
        let key = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("sec-websocket-key").then(|| value.trim())
            })
            .unwrap();
        let accept = derive_accept_key(key.as_bytes());
        stream
            .write_all(
                format!(
                    "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut socket = WebSocketStream::from_raw_socket(stream, Role::Server, None).await;
        while let Some(Ok(message)) = socket.next().await {
            if message.is_close() {
                break;
            }
            socket.send(message).await.unwrap();
        }
    });
    let fixture = Fixture::new(format!("http://{upstream_address}"));
    let main = MainGateway::bind(fixture.config).await.unwrap();
    let mut roots = RootCertStore::empty();
    roots.add(fixture.server_ca.clone()).unwrap();
    let client = ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .with_root_certificates(roots)
        .with_no_client_auth();
    let (mut socket, _) = connect_async_tls_with_config(
        format!("wss://localhost:{}/chat", main.local_addr().port()),
        None,
        false,
        Some(Connector::Rustls(Arc::new(client))),
    )
    .await
    .unwrap();
    socket.send(Message::Text("hello".into())).await.unwrap();
    assert_eq!(socket.next().await.unwrap().unwrap(), Message::Text("hello".into()));
    socket.send(Message::Binary(vec![0, 1, 2, 255].into())).await.unwrap();
    assert_eq!(socket.next().await.unwrap().unwrap(), Message::Binary(vec![0, 1, 2, 255].into()));
    socket.close(None).await.unwrap();
    task.await.unwrap();
    main.shutdown().await;
}

async fn request(address: std::net::SocketAddr, ca: &CertificateDer<'static>, path: &str) -> String {
    let mut stream = tls_stream(address, ca).await;
    stream
        .write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    String::from_utf8(response).unwrap()
}

async fn tls_stream(
    address: std::net::SocketAddr,
    ca: &CertificateDer<'static>,
) -> tokio_rustls::client::TlsStream<TcpStream> {
    let mut roots = RootCertStore::empty();
    roots.add(ca.clone()).unwrap();
    let client = ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .with_root_certificates(roots)
        .with_no_client_auth();
    let stream = TcpStream::connect(address).await.unwrap();
    TlsConnector::from(Arc::new(client)).connect(ServerName::try_from("localhost").unwrap(), stream).await.unwrap()
}

async fn read_headers(stream: &mut TcpStream) -> String {
    let mut request = Vec::new();
    let mut byte = [0_u8; 1];
    while !request.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).await.unwrap();
        request.push(byte[0]);
    }
    String::from_utf8(request).unwrap()
}
