use std::collections::BTreeMap;
use std::fs;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::{GatewayError, GatewayErrorCode};

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum GatewayConfig {
    Main(Box<MainConfig>),
    Edge(Box<EdgeConfig>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MainConfig {
    pub listen: String,
    pub certificate: PathBuf,
    pub private_key: PathBuf,
    pub edge_ca_certificate: PathBuf,
    pub client_ca_certificate: PathBuf,
    #[serde(default = "default_edge_path")]
    pub edge_path: String,
    #[serde(default = "default_dbx_path")]
    pub dbx_path: String,
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,
    #[serde(default = "default_tls_handshake_timeout_secs")]
    pub tls_handshake_timeout_secs: u64,
    #[serde(default = "default_http_header_timeout_secs")]
    pub http_header_timeout_secs: u64,
    #[serde(default)]
    pub enrollment: Option<MainEnrollmentConfig>,
    #[serde(default)]
    pub allowed_edge_ids: Vec<String>,
    #[serde(default)]
    pub revoked_edge_serials: Vec<String>,
    #[serde(default)]
    pub client_route_acl: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub fallback_upstream: Option<String>,
    #[serde(default)]
    pub health_listen: Option<String>,
    #[serde(default)]
    pub state_file: Option<PathBuf>,
    #[serde(default = "default_max_streams_per_edge")]
    pub max_streams_per_edge: usize,
    #[serde(default = "default_max_streams_per_client")]
    pub max_streams_per_client: usize,
    #[serde(default = "default_connection_rate_per_second")]
    pub connection_rate_per_second: u32,
    #[serde(default = "default_connection_rate_burst")]
    pub connection_rate_burst: u32,
    #[serde(default = "default_global_buffer_budget_bytes")]
    pub global_buffer_budget_bytes: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MainEnrollmentConfig {
    #[serde(default = "default_enrollment_path")]
    pub path: String,
    #[serde(default = "default_renewal_path")]
    pub renewal_path: String,
    pub allowed_edge_ids: Vec<String>,
    pub pki: PkiEndpointConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum PkiEndpointConfig {
    Unix {
        unix_socket: PathBuf,
    },
    Remote {
        remote_address: String,
        server_name: String,
        ca_certificate: PathBuf,
        certificate: PathBuf,
        private_key: PathBuf,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeConfig {
    pub edge_id: String,
    pub main_url: String,
    pub certificate: PathBuf,
    pub private_key: PathBuf,
    pub ca_certificate: PathBuf,
    pub targets: BTreeMap<String, EdgeTarget>,
    #[serde(default)]
    pub bootstrap: Option<EdgeBootstrapConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeBootstrapConfig {
    pub token_file: PathBuf,
    pub enrollment_url: String,
    pub server_spki_sha256: String,
    #[serde(default = "default_renew_before_days")]
    pub renew_before_days: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeTarget {
    #[serde(default)]
    pub display_name: String,
    pub address: TargetAddress,
    #[serde(default)]
    pub allow_remote: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetAddress {
    Tcp { tcp: String },
    Unix { unix: PathBuf },
}

#[derive(Deserialize)]
#[serde(untagged)]
enum TargetAddressWire {
    TcpString(String),
    Tcp(TcpAddressWire),
    Unix(UnixAddressWire),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TcpAddressWire {
    tcp: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UnixAddressWire {
    unix: PathBuf,
}

impl<'de> Deserialize<'de> for TargetAddress {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(match TargetAddressWire::deserialize(deserializer)? {
            TargetAddressWire::TcpString(tcp) | TargetAddressWire::Tcp(TcpAddressWire { tcp }) => Self::Tcp { tcp },
            TargetAddressWire::Unix(UnixAddressWire { unix }) => Self::Unix { unix },
        })
    }
}

pub fn load_config_file(path: &Path) -> Result<GatewayConfig, GatewayError> {
    let path = absolute_path(path)?;
    let contents = fs::read_to_string(&path).map_err(|_| config_error("configuration file could not be read"))?;
    let mut config: GatewayConfig = toml::from_str(&contents)
        .map_err(|error| config_error(format!("configuration is invalid: {}", error.message())))?;
    let base_dir = path.parent().unwrap_or_else(|| Path::new("."));

    resolve_paths(&mut config, base_dir);
    validate_config(&config)?;
    Ok(config)
}

fn absolute_path(path: &Path) -> Result<PathBuf, GatewayError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|current_dir| current_dir.join(path))
            .map_err(|_| config_error("configuration path could not be resolved"))?
    };
    fs::canonicalize(absolute).map_err(|_| config_error("configuration path could not be resolved"))
}

fn resolve_paths(config: &mut GatewayConfig, base_dir: &Path) {
    match config {
        GatewayConfig::Main(main) => {
            resolve_path(&mut main.certificate, base_dir);
            resolve_path(&mut main.private_key, base_dir);
            resolve_path(&mut main.edge_ca_certificate, base_dir);
            resolve_path(&mut main.client_ca_certificate, base_dir);
            if let Some(state_file) = &mut main.state_file {
                resolve_path(state_file, base_dir);
            }
            if let Some(enrollment) = &mut main.enrollment {
                match &mut enrollment.pki {
                    PkiEndpointConfig::Unix { unix_socket } => resolve_path(unix_socket, base_dir),
                    PkiEndpointConfig::Remote { ca_certificate, certificate, private_key, .. } => {
                        resolve_path(ca_certificate, base_dir);
                        resolve_path(certificate, base_dir);
                        resolve_path(private_key, base_dir);
                    }
                }
            }
        }
        GatewayConfig::Edge(edge) => {
            resolve_path(&mut edge.certificate, base_dir);
            resolve_path(&mut edge.private_key, base_dir);
            resolve_path(&mut edge.ca_certificate, base_dir);
            if let Some(bootstrap) = &mut edge.bootstrap {
                resolve_path(&mut bootstrap.token_file, base_dir);
            }
            for (name, target) in &mut edge.targets {
                if target.display_name.is_empty() {
                    target.display_name.clone_from(name);
                }
                if let TargetAddress::Unix { unix } = &mut target.address {
                    resolve_path(unix, base_dir);
                }
            }
        }
    }
}

fn resolve_path(path: &mut PathBuf, base_dir: &Path) {
    if path.is_relative() {
        *path = base_dir.join(&*path);
    }
}

fn validate_config(config: &GatewayConfig) -> Result<(), GatewayError> {
    match config {
        GatewayConfig::Main(main) => {
            validate_regular_file(&main.certificate, "certificate")?;
            validate_regular_file(&main.edge_ca_certificate, "edge_ca_certificate")?;
            validate_regular_file(&main.client_ca_certificate, "client_ca_certificate")?;
            validate_regular_file(&main.private_key, "private_key")?;
            validate_private_key_permissions(&main.private_key)?;
            if !valid_reserved_path(&main.edge_path)
                || !valid_reserved_path(&main.dbx_path)
                || main.edge_path == main.dbx_path
            {
                return Err(config_error("reserved paths must be distinct absolute paths"));
            }
            if main.max_connections == 0
                || main.max_streams_per_edge == 0
                || main.max_streams_per_client == 0
                || main.connection_rate_per_second == 0
                || main.connection_rate_burst == 0
                || main.global_buffer_budget_bytes < 2 * 1024 * 1024
                || main.tls_handshake_timeout_secs == 0
                || main.http_header_timeout_secs == 0
            {
                return Err(config_error("connection limits and timeouts must be greater than zero"));
            }
            if main.client_route_acl.iter().any(|(client_id, routes)| {
                !valid_identity(client_id)
                    || routes.is_empty()
                    || routes.iter().any(|route| !valid_client_route_rule(route))
            }) {
                return Err(config_error("client_route_acl contains an invalid client identity or route rule"));
            }
            if let Some(enrollment) = &main.enrollment {
                if !valid_reserved_path(&enrollment.path)
                    || !valid_reserved_path(&enrollment.renewal_path)
                    || enrollment.path == main.edge_path
                    || enrollment.path == main.dbx_path
                    || enrollment.renewal_path == enrollment.path
                    || enrollment.renewal_path == main.edge_path
                    || enrollment.renewal_path == main.dbx_path
                    || enrollment.allowed_edge_ids.is_empty()
                    || enrollment.allowed_edge_ids.iter().any(|id| !valid_identity(id))
                {
                    return Err(config_error("enrollment path and allowed Edge IDs are invalid"));
                }
                if let PkiEndpointConfig::Remote { remote_address, ca_certificate, certificate, private_key, .. } =
                    &enrollment.pki
                {
                    remote_address.parse::<SocketAddr>().map_err(|_| config_error("remote PKI address is invalid"))?;
                    validate_credentials(certificate, private_key, ca_certificate)?;
                }
            }
            if main.allowed_edge_ids.iter().any(|id| !valid_identity(id))
                || main.revoked_edge_serials.iter().any(|serial| {
                    serial.is_empty() || !serial.bytes().all(|byte| byte.is_ascii_hexdigit() || byte == b':')
                })
            {
                return Err(config_error("Edge ACL or revoked serial list is invalid"));
            }
            if let Some(upstream) = &main.fallback_upstream {
                validate_fallback_upstream(upstream)?;
            }
            if let Some(health) = &main.health_listen {
                let address: SocketAddr =
                    health.parse().map_err(|_| config_error("health listen address is invalid"))?;
                if !address.ip().is_loopback() {
                    return Err(config_error("health listen address must be loopback"));
                }
            }
            Ok(())
        }
        GatewayConfig::Edge(edge) => {
            if !valid_identity(&edge.edge_id) {
                return Err(config_error("configuration edge_id must not be empty"));
            }
            if edge.main_url.strip_prefix("wss://").is_none_or(str::is_empty) {
                return Err(config_error("configuration main_url must use wss://"));
            }
            for target in edge.targets.values() {
                validate_target(target)?;
            }
            validate_regular_file(&edge.ca_certificate, "ca_certificate")?;
            let enrolled = edge.certificate.is_file() && edge.private_key.is_file();
            if enrolled {
                validate_credentials(&edge.certificate, &edge.private_key, &edge.ca_certificate)
            } else if let Some(bootstrap) = &edge.bootstrap {
                validate_regular_file(&bootstrap.token_file, "bootstrap token_file")?;
                validate_private_key_permissions(&bootstrap.token_file)?;
                if !bootstrap.enrollment_url.starts_with("https://")
                    || hex::decode(&bootstrap.server_spki_sha256).is_err()
                    || bootstrap.server_spki_sha256.len() != 64
                {
                    return Err(config_error("Edge bootstrap URL or SPKI pin is invalid"));
                }
                Ok(())
            } else {
                Err(config_error("Edge credentials are unavailable and bootstrap is not configured"))
            }
        }
    }
}

fn valid_client_route_rule(rule: &str) -> bool {
    let Some((edge_id, target_id)) = rule.split_once('/') else { return false };
    !edge_id.is_empty()
        && !target_id.is_empty()
        && !target_id.contains('/')
        && (edge_id == "*" || valid_identity(edge_id))
        && (target_id == "*" || valid_identity(target_id))
}

fn default_edge_path() -> String {
    "/_dbx/edge".to_string()
}

fn default_dbx_path() -> String {
    "/_dbx/client".to_string()
}

fn default_enrollment_path() -> String {
    "/_dbx/enroll".to_string()
}

fn default_renew_before_days() -> u64 {
    30
}

fn default_renewal_path() -> String {
    "/_dbx/renew".to_string()
}

fn valid_identity(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn validate_fallback_upstream(value: &str) -> Result<(), GatewayError> {
    let uri: hyper::Uri = value.parse().map_err(|_| config_error("fallback upstream URL is invalid"))?;
    if !matches!(uri.scheme_str(), Some("http" | "https"))
        || uri.authority().is_none()
        || uri.authority().is_some_and(|authority| authority.as_str().contains('@'))
    {
        return Err(config_error("fallback upstream URL is invalid"));
    }
    Ok(())
}

fn default_max_connections() -> usize {
    1024
}

fn default_max_streams_per_edge() -> usize {
    256
}

fn default_max_streams_per_client() -> usize {
    32
}

fn default_connection_rate_per_second() -> u32 {
    64
}

fn default_connection_rate_burst() -> u32 {
    128
}

fn default_global_buffer_budget_bytes() -> usize {
    256 * 1024 * 1024
}

fn default_tls_handshake_timeout_secs() -> u64 {
    10
}

fn default_http_header_timeout_secs() -> u64 {
    10
}

fn valid_reserved_path(path: &str) -> bool {
    path.starts_with('/') && !path.starts_with("//") && !path.contains(['?', '#'])
}

fn validate_target(target: &EdgeTarget) -> Result<(), GatewayError> {
    let TargetAddress::Tcp { tcp } = &target.address else {
        return Ok(());
    };
    let host = tcp_host(tcp)?;
    let is_loopback =
        host.eq_ignore_ascii_case("localhost") || host.parse::<IpAddr>().is_ok_and(|address| address.is_loopback());

    if !is_loopback && !target.allow_remote {
        return Err(config_error("remote TCP targets require allow_remote=true in the configuration"));
    }
    Ok(())
}

fn tcp_host(address: &str) -> Result<String, GatewayError> {
    if let Ok(socket) = address.parse::<SocketAddr>() {
        if socket.port() == 0 {
            return Err(config_error("target TCP port must be between 1 and 65535"));
        }
        return Ok(socket.ip().to_string());
    }

    let (host, port) =
        address.rsplit_once(':').ok_or_else(|| config_error("target TCP address must contain a host and port"))?;
    let host = host.strip_prefix('[').and_then(|value| value.strip_suffix(']')).unwrap_or(host);
    if host.is_empty() || !port.parse::<u16>().is_ok_and(|port| port > 0) {
        return Err(config_error("target TCP address must contain a valid host and port"));
    }
    Ok(host.to_string())
}

fn validate_credentials(certificate: &Path, private_key: &Path, ca_certificate: &Path) -> Result<(), GatewayError> {
    validate_regular_file(certificate, "certificate")?;
    validate_regular_file(ca_certificate, "ca_certificate")?;
    validate_regular_file(private_key, "private_key")?;
    validate_private_key_permissions(private_key)
}

fn validate_regular_file(path: &Path, field: &str) -> Result<(), GatewayError> {
    validate_no_symlink_components(path, field)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(()),
        _ => Err(config_error(format!("configuration {field} must reference an existing regular file"))),
    }
}

fn validate_no_symlink_components(path: &Path, field: &str) -> Result<(), GatewayError> {
    use std::path::Component;

    let mut current = PathBuf::new();
    for component in path.components() {
        if matches!(component, Component::ParentDir) {
            return Err(config_error(format!("configuration {field} path must not contain parent traversal")));
        }
        current.push(component);
        let metadata = fs::symlink_metadata(&current)
            .map_err(|_| config_error(format!("configuration {field} path could not be inspected")))?;
        if metadata.file_type().is_symlink() {
            return Err(config_error(format!("configuration {field} path must not contain symbolic links")));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn validate_private_key_permissions(path: &Path) -> Result<(), GatewayError> {
    use std::os::unix::fs::PermissionsExt;

    let mode = fs::symlink_metadata(path)
        .map_err(|_| config_error("configuration private_key metadata could not be read"))?
        .permissions()
        .mode()
        & 0o7777;
    if mode != 0o600 {
        return Err(config_error("configuration private_key permissions must be 0600"));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_private_key_permissions(_path: &Path) -> Result<(), GatewayError> {
    Ok(())
}

fn config_error(message: impl Into<String>) -> GatewayError {
    GatewayError { code: GatewayErrorCode::ConfigInvalid, message: message.into() }
}
