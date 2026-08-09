#![cfg(feature = "server")]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use dbx_gateway::config::{load_config_file, GatewayConfig, TargetAddress};
use dbx_gateway::{run_gateway_command, GatewayCommand, GatewayErrorCode};

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let id = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("dbx-gateway-config-{}-{id}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_file(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

#[cfg(unix)]
fn make_private_key(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    write_file(path, "test private key");
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

#[cfg(not(unix))]
fn make_private_key(path: &Path) {
    write_file(path, "test private key");
}

fn write_credentials(dir: &Path) {
    write_file(&dir.join("certs/server.pem"), "test certificate");
    write_file(&dir.join("certs/root.pem"), "test CA certificate");
    make_private_key(&dir.join("certs/server.key"));
}

fn main_config(extra: &str) -> String {
    format!(
        r#"mode = "main"
listen = "127.0.0.1:8443"
certificate = "certs/server.pem"
private_key = "certs/server.key"
ca_certificate = "certs/root.pem"
{extra}
"#
    )
}

fn edge_config(targets: &str) -> String {
    format!(
        r#"mode = "edge"
edge_id = "edge-prod-01"
main_url = "wss://main.example.com/_dbx/control"
certificate = "certs/server.pem"
private_key = "certs/server.key"
ca_certificate = "certs/root.pem"
{targets}
"#
    )
}

#[test]
fn rejects_unknown_fields() {
    let dir = TempDir::new();
    write_credentials(dir.path());
    let config_path = dir.path().join("gateway.toml");
    write_file(&config_path, &main_config("unexpected = true"));

    let error = load_config_file(&config_path).unwrap_err();

    assert_eq!(error.code, GatewayErrorCode::ConfigInvalid);
    assert!(error.message.contains("unknown field"));
}

#[test]
fn rejects_main_without_server_certificate_or_key() {
    let dir = TempDir::new();
    let config_path = dir.path().join("gateway.toml");
    write_file(
        &config_path,
        r#"mode = "main"
listen = "127.0.0.1:8443"
ca_certificate = "certs/root.pem"
"#,
    );

    let error = load_config_file(&config_path).unwrap_err();

    assert_eq!(error.code, GatewayErrorCode::ConfigInvalid);
    assert!(error.message.contains("certificate") || error.message.contains("private_key"));
}

#[test]
fn accepts_loopback_tcp_and_unix_targets_by_default() {
    let dir = TempDir::new();
    write_credentials(dir.path());
    let config_path = dir.path().join("gateway.toml");
    write_file(
        &config_path,
        &edge_config(
            r#"[targets.postgres]
address = "127.4.3.2:5432"

[targets.mysql]
address = { tcp = "localhost:3306" }

[targets.redis]
address = { unix = "run/redis.sock" }
"#,
        ),
    );

    let config = load_config_file(&config_path).unwrap();
    let GatewayConfig::Edge(edge) = config else {
        panic!("expected edge config");
    };

    assert_eq!(edge.targets["postgres"].display_name, "postgres");
    assert_eq!(edge.targets["postgres"].address, TargetAddress::Tcp { tcp: "127.4.3.2:5432".to_string() });
    assert_eq!(edge.targets["redis"].address, TargetAddress::Unix { unix: dir.path().join("run/redis.sock") });
}

#[test]
fn rejects_remote_tcp_without_explicit_opt_in() {
    let dir = TempDir::new();
    write_credentials(dir.path());
    let config_path = dir.path().join("gateway.toml");
    write_file(
        &config_path,
        &edge_config(
            r#"[targets.postgres]
address = "10.0.0.8:5432"
"#,
        ),
    );

    let error = load_config_file(&config_path).unwrap_err();

    assert_eq!(error.code, GatewayErrorCode::ConfigInvalid);
    assert!(error.message.contains("allow_remote"));
}

#[test]
fn accepts_remote_tcp_with_explicit_opt_in() {
    let dir = TempDir::new();
    write_credentials(dir.path());
    let config_path = dir.path().join("gateway.toml");
    write_file(
        &config_path,
        &edge_config(
            r#"[targets.postgres]
address = "db.internal.example:5432"
allow_remote = true
"#,
        ),
    );

    assert!(load_config_file(&config_path).is_ok());
}

#[test]
fn resolves_relative_credential_paths_from_config_directory() {
    let dir = TempDir::new();
    write_credentials(dir.path());
    let config_path = dir.path().join("gateway.toml");
    write_file(&config_path, &main_config(""));

    let config = load_config_file(&config_path).unwrap();
    let GatewayConfig::Main(main) = config else {
        panic!("expected main config");
    };

    assert_eq!(main.certificate, dir.path().join("certs/server.pem"));
    assert_eq!(main.private_key, dir.path().join("certs/server.key"));
    assert_eq!(main.ca_certificate, dir.path().join("certs/root.pem"));
}

#[cfg(unix)]
#[test]
fn rejects_private_key_permissions_other_than_0600() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new();
    write_credentials(dir.path());
    fs::set_permissions(dir.path().join("certs/server.key"), fs::Permissions::from_mode(0o640)).unwrap();
    let config_path = dir.path().join("gateway.toml");
    write_file(&config_path, &main_config(""));

    let error = load_config_file(&config_path).unwrap_err();

    assert_eq!(error.code, GatewayErrorCode::ConfigInvalid);
    assert!(error.message.contains("0600"));
}

#[cfg(unix)]
#[test]
fn rejects_private_key_permissions_with_special_bits() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new();
    write_credentials(dir.path());
    fs::set_permissions(dir.path().join("certs/server.key"), fs::Permissions::from_mode(0o1600)).unwrap();
    let config_path = dir.path().join("gateway.toml");
    write_file(&config_path, &main_config(""));

    let error = load_config_file(&config_path).unwrap_err();

    assert_eq!(error.code, GatewayErrorCode::ConfigInvalid);
    assert!(error.message.contains("0600"));
}

#[test]
fn check_config_uses_loader_and_maps_results_to_exit_codes() {
    let dir = TempDir::new();
    write_credentials(dir.path());
    let valid_path = dir.path().join("valid.toml");
    write_file(&valid_path, &main_config(""));

    let success = run_gateway_command(GatewayCommand::CheckConfig, &valid_path);
    assert_eq!(success.exit_code, 0);
    assert!(success.message.contains("valid"));

    let missing_path = dir.path().join("missing.toml");
    let failure = run_gateway_command(GatewayCommand::CheckConfig, &missing_path);
    assert_eq!(failure.exit_code, 2);
    assert!(failure.message.contains("configuration"));

    let runtime_failure = run_gateway_command(GatewayCommand::Serve, &valid_path);
    assert_eq!(runtime_failure.exit_code, 1);
    assert!(runtime_failure.message.contains("not implemented"));
}
