use std::fs::File;
#[cfg(not(unix))]
use std::fs::OpenOptions;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, UnixTime};
use rustls::server::danger::ClientCertVerifier;
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig};
use sha2::{Digest, Sha256};
use x509_parser::extensions::GeneralName;
use x509_parser::prelude::parse_x509_certificate;

use crate::config::MainConfig;
use crate::{GatewayError, GatewayErrorCode};

#[derive(Clone)]
pub enum PeerIdentity {
    Anonymous,
    Edge { edge_id: String, serial: String, fingerprint_sha256: [u8; 32] },
    DbxClient { client_id: String, serial: String, fingerprint_sha256: [u8; 32] },
}

pub(crate) struct GatewayTls {
    pub server_config: Arc<ServerConfig>,
    edge_verifier: Arc<dyn ClientCertVerifier>,
    client_verifier: Arc<dyn ClientCertVerifier>,
}

impl GatewayTls {
    pub(crate) fn load(config: &MainConfig) -> Result<Self, GatewayError> {
        let edge_roots = load_roots(&config.edge_ca_certificate)?;
        let client_roots = load_roots(&config.client_ca_certificate)?;
        let edge_verifier = WebPkiClientVerifier::builder(Arc::new(edge_roots.clone()))
            .build()
            .map_err(|_| tls_error("could not configure Edge CA"))?;
        let client_verifier = WebPkiClientVerifier::builder(Arc::new(client_roots.clone()))
            .build()
            .map_err(|_| tls_error("could not configure DBX Client CA"))?;
        let mut combined = edge_roots;
        for root in client_roots.roots {
            if !combined.roots.contains(&root) {
                combined.roots.push(root);
            }
        }
        let verifier = WebPkiClientVerifier::builder(Arc::new(combined))
            .allow_unauthenticated()
            .build()
            .map_err(|_| tls_error("could not configure client authentication"))?;
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let mut server_config = ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|_| tls_error("could not restrict TLS protocol version"))?
            .with_client_cert_verifier(verifier)
            .with_single_cert(load_certificates(&config.certificate)?, load_private_key(&config.private_key)?)
            .map_err(|_| tls_error("server certificate or private key was rejected"))?;
        server_config.max_early_data_size = 0;
        server_config.send_half_rtt_data = false;
        server_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Ok(Self { server_config: Arc::new(server_config), edge_verifier, client_verifier })
    }

    pub(crate) fn classify(
        &self,
        certificates: Option<&[CertificateDer<'static>]>,
    ) -> Result<PeerIdentity, GatewayError> {
        let Some((leaf, intermediates)) = certificates.and_then(|certificates| certificates.split_first()) else {
            return Ok(PeerIdentity::Anonymous);
        };
        let now = UnixTime::now();
        let edge = self.edge_verifier.verify_client_cert(leaf, intermediates, now).is_ok();
        let client = self.client_verifier.verify_client_cert(leaf, intermediates, now).is_ok();
        match (edge, client) {
            (true, false) => identity(leaf, "urn:dbx-gateway:edge:", true),
            (false, true) => identity(leaf, "urn:dbx-gateway:client:", false),
            _ => Err(tls_error("peer identity was rejected")),
        }
    }
}

fn identity(leaf: &CertificateDer<'_>, prefix: &str, edge: bool) -> Result<PeerIdentity, GatewayError> {
    let (_, certificate) =
        parse_x509_certificate(leaf.as_ref()).map_err(|_| tls_error("peer identity was rejected"))?;
    let eku = certificate
        .extended_key_usage()
        .map_err(|_| tls_error("peer identity was rejected"))?
        .ok_or_else(|| tls_error("peer identity was rejected"))?;
    let eku = &eku.value;
    if !eku.client_auth
        || eku.any
        || eku.server_auth
        || eku.code_signing
        || eku.email_protection
        || eku.time_stamping
        || eku.ocsp_signing
        || !eku.other.is_empty()
    {
        return Err(tls_error("peer identity was rejected"));
    }
    let sans = certificate
        .subject_alternative_name()
        .map_err(|_| tls_error("peer identity was rejected"))?
        .ok_or_else(|| tls_error("peer identity was rejected"))?;
    let uris = sans
        .value
        .general_names
        .iter()
        .filter_map(|name| match name {
            GeneralName::URI(uri) => Some(*uri),
            _ => None,
        })
        .collect::<Vec<_>>();
    let [uri] = uris.as_slice() else {
        return Err(tls_error("peer identity was rejected"));
    };
    let id =
        uri.strip_prefix(prefix).filter(|id| valid_id(id)).ok_or_else(|| tls_error("peer identity was rejected"))?;
    let serial = certificate.raw_serial_as_string();
    let fingerprint_sha256: [u8; 32] = Sha256::digest(leaf.as_ref()).into();
    Ok(if edge {
        PeerIdentity::Edge { edge_id: id.to_string(), serial, fingerprint_sha256 }
    } else {
        PeerIdentity::DbxClient { client_id: id.to_string(), serial, fingerprint_sha256 }
    })
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

pub(crate) fn load_certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>, GatewayError> {
    let file = open_regular_file(path, None, "certificate file")?;
    let certificates = rustls_pemfile::certs(&mut BufReader::new(file))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| tls_error("certificate file was rejected"))?;
    if certificates.is_empty() {
        Err(tls_error("certificate file was empty"))
    } else {
        Ok(certificates)
    }
}

pub(crate) fn load_roots(path: &Path) -> Result<RootCertStore, GatewayError> {
    let mut roots = RootCertStore::empty();
    for certificate in load_certificates(path)? {
        roots.add(certificate).map_err(|_| tls_error("CA certificate was rejected"))?;
    }
    Ok(roots)
}

pub(crate) fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>, GatewayError> {
    let file = open_regular_file(path, Some(0o600), "private key file")?;
    rustls_pemfile::private_key(&mut BufReader::new(file))
        .map_err(|_| tls_error("private key file was rejected"))?
        .ok_or_else(|| tls_error("private key file was empty"))
}

#[cfg(unix)]
fn open_regular_file(path: &Path, expected_mode: Option<u32>, label: &str) -> Result<File, GatewayError> {
    use std::ffi::{CString, OsString};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Component;

    let mut names = Vec::<OsString>::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => return Err(tls_error(&format!("{label} path was rejected"))),
            Component::Normal(name) => names.push(name.to_os_string()),
            Component::Prefix(_) => return Err(tls_error(&format!("{label} path was rejected"))),
        }
    }
    if names.is_empty() {
        return Err(tls_error(&format!("{label} path was rejected")));
    }

    let mut current = File::open(if path.is_absolute() { "/" } else { "." })
        .map_err(|_| tls_error(&format!("{label} path could not be opened")))?;
    let last = names.len() - 1;
    for (index, name) in names.into_iter().enumerate() {
        let name = CString::new(name.as_bytes()).map_err(|_| tls_error(&format!("{label} path was rejected")))?;
        let mut flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW;
        if index != last {
            flags |= libc::O_DIRECTORY;
        }
        // Each component is opened relative to the already verified directory descriptor.
        let descriptor = unsafe { libc::openat(current.as_raw_fd(), name.as_ptr(), flags) };
        if descriptor < 0 {
            return Err(tls_error(&format!("{label} path was rejected")));
        }
        current = unsafe { File::from_raw_fd(descriptor) };
    }
    validate_opened_file(current, expected_mode, label)
}

#[cfg(not(unix))]
fn open_regular_file(path: &Path, expected_mode: Option<u32>, label: &str) -> Result<File, GatewayError> {
    let mut options = OpenOptions::new();
    options.read(true);
    let file = options.open(path).map_err(|_| tls_error(&format!("{label} could not be read")))?;
    validate_opened_file(file, expected_mode, label)
}

fn validate_opened_file(file: File, expected_mode: Option<u32>, label: &str) -> Result<File, GatewayError> {
    let metadata = file.metadata().map_err(|_| tls_error(&format!("{label} could not be inspected")))?;
    if !metadata.is_file() {
        return Err(tls_error(&format!("{label} was rejected")));
    }
    #[cfg(unix)]
    if let Some(expected_mode) = expected_mode {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o7777 != expected_mode {
            return Err(tls_error(&format!("{label} permissions were rejected")));
        }
    }
    Ok(file)
}

fn tls_error(message: &str) -> GatewayError {
    GatewayError { code: GatewayErrorCode::TlsRejected, message: message.to_string() }
}
