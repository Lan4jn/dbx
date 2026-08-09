use std::collections::BTreeMap;
use std::fs;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::{GatewayError, GatewayErrorCode};

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum GatewayConfig {
    Main(MainConfig),
    Edge(EdgeConfig),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MainConfig {
    pub listen: String,
    pub certificate: PathBuf,
    pub private_key: PathBuf,
    pub ca_certificate: PathBuf,
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
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }

    std::env::current_dir()
        .map(|current_dir| current_dir.join(path))
        .map_err(|_| config_error("configuration path could not be resolved"))
}

fn resolve_paths(config: &mut GatewayConfig, base_dir: &Path) {
    match config {
        GatewayConfig::Main(main) => {
            resolve_path(&mut main.certificate, base_dir);
            resolve_path(&mut main.private_key, base_dir);
            resolve_path(&mut main.ca_certificate, base_dir);
        }
        GatewayConfig::Edge(edge) => {
            resolve_path(&mut edge.certificate, base_dir);
            resolve_path(&mut edge.private_key, base_dir);
            resolve_path(&mut edge.ca_certificate, base_dir);
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
        GatewayConfig::Main(main) => validate_credentials(&main.certificate, &main.private_key, &main.ca_certificate),
        GatewayConfig::Edge(edge) => {
            if edge.edge_id.trim().is_empty() {
                return Err(config_error("configuration edge_id must not be empty"));
            }
            if edge.main_url.strip_prefix("wss://").is_none_or(str::is_empty) {
                return Err(config_error("configuration main_url must use wss://"));
            }
            for target in edge.targets.values() {
                validate_target(target)?;
            }
            validate_credentials(&edge.certificate, &edge.private_key, &edge.ca_certificate)
        }
    }
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
        return Ok(socket.ip().to_string());
    }

    let (host, port) =
        address.rsplit_once(':').ok_or_else(|| config_error("target TCP address must contain a host and port"))?;
    let host = host.strip_prefix('[').and_then(|value| value.strip_suffix(']')).unwrap_or(host);
    if host.is_empty() || port.parse::<u16>().is_err() {
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
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(()),
        _ => Err(config_error(format!("configuration {field} must reference an existing regular file"))),
    }
}

#[cfg(unix)]
fn validate_private_key_permissions(path: &Path) -> Result<(), GatewayError> {
    use std::os::unix::fs::PermissionsExt;

    let mode = fs::metadata(path)
        .map_err(|_| config_error("configuration private_key metadata could not be read"))?
        .permissions()
        .mode()
        & 0o777;
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
