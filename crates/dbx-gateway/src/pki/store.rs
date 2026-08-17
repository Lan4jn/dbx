use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use pkcs8::der::Decode;
use pkcs8::{LineEnding, PrivateKeyInfoRef};
use rcgen::{
    BasicConstraints, CertificateParams, CertificateRevocationListParams, DnType, IsCa, Issuer, KeyIdMethod, KeyPair,
    KeyUsagePurpose, RevocationReason as RcgenRevocationReason, RevokedCertParams, SerialNumber,
};
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};
use zeroize::{Zeroize, Zeroizing};

use super::{CertificateRole, RevocationReason};
use crate::{GatewayError, GatewayErrorCode};

const ROOT: &str = "root";
const MAX_SERIAL_BYTES: usize = 20;
static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
pub struct PkiStore {
    pub(crate) data_dir: PathBuf,
}

#[derive(Debug)]
pub struct GeneratedCrl {
    pub number: u64,
    pub pem: String,
}

#[derive(Deserialize, Serialize)]
struct IssuedRecord {
    serial_hex: String,
    certificate_pem: String,
    identity: String,
    role: String,
    revoked: bool,
    revoked_at: Option<i64>,
    reason: Option<String>,
}

impl PkiStore {
    pub fn init(data_dir: &Path, password: &Zeroizing<String>) -> Result<Self, GatewayError> {
        require_nonempty_secret(password, "CA password")?;
        let _initialization_lock = lock_initialization(data_dir)?;
        reject_existing_pki(data_dir)?;
        let staging_dir = create_initialization_dir(data_dir)?;

        let result = (|| {
            let root_key = KeyPair::generate().map_err(|_| pki_error("could not generate root CA"))?;
            let root_params = ca_params("DBX Gateway Root CA", BasicConstraints::Constrained(1));
            let root_cert = root_params.self_signed(&root_key).map_err(|_| pki_error("could not create root CA"))?;
            let root_issuer = Issuer::new(root_params, root_key);

            write_ca(&staging_dir, ROOT, &root_cert.pem(), root_issuer.key(), password)?;
            for (role, common_name) in [
                (CertificateRole::Server, "DBX Gateway Server CA"),
                (CertificateRole::Edge, "DBX Gateway Edge CA"),
                (CertificateRole::Client, "DBX Gateway Client CA"),
            ] {
                let key = KeyPair::generate().map_err(|_| pki_error("could not generate intermediate CA"))?;
                let params = ca_params(common_name, BasicConstraints::Constrained(0));
                let cert =
                    params.signed_by(&key, &root_issuer).map_err(|_| pki_error("could not create intermediate CA"))?;
                write_ca(&staging_dir, role.as_str(), &cert.pem(), &key, password)?;
            }
            publish_initialized_pki(&staging_dir, data_dir)
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&staging_dir);
        }
        result?;

        Ok(Self { data_dir: data_dir.to_path_buf() })
    }

    pub fn open(data_dir: &Path) -> Result<Self, GatewayError> {
        validate_existing_pki(data_dir)?;
        Ok(Self { data_dir: data_dir.to_path_buf() })
    }

    pub fn open_online_edge(data_dir: &Path) -> Result<Self, GatewayError> {
        validate_directory(data_dir, "PKI data directory")?;
        validate_existing_role(data_dir, CertificateRole::Edge.as_str())?;
        Ok(Self { data_dir: data_dir.to_path_buf() })
    }

    pub fn root_certificate_path(&self) -> PathBuf {
        certificate_path(&self.data_dir, ROOT)
    }

    pub fn server_ca_certificate_path(&self) -> PathBuf {
        certificate_path(&self.data_dir, CertificateRole::Server.as_str())
    }

    pub fn edge_ca_certificate_path(&self) -> PathBuf {
        certificate_path(&self.data_dir, CertificateRole::Edge.as_str())
    }

    pub fn client_ca_certificate_path(&self) -> PathBuf {
        certificate_path(&self.data_dir, CertificateRole::Client.as_str())
    }

    pub fn revoke(
        &self,
        role: CertificateRole,
        serial_hex: &str,
        reason: RevocationReason,
        ca_password: &Zeroizing<String>,
    ) -> Result<GeneratedCrl, GatewayError> {
        require_nonempty_secret(ca_password, "CA password")?;
        let serial_hex = normalize_serial_hex(serial_hex)?;
        let _role_lock = lock_role(&self.data_dir, role)?;
        let mut record = read_issued_record(&self.data_dir, role, &serial_hex)?;
        if record.revoked {
            if record.reason.as_deref() != Some(reason.as_str()) {
                return Err(pki_error("certificate serial is already revoked with a different reason"));
            }
        } else {
            record.revoked = true;
            record.revoked_at = Some(OffsetDateTime::now_utc().unix_timestamp());
            record.reason = Some(reason.as_str().to_string());
        }

        let mut records = read_issued_records(&self.data_dir, role)?;
        let replaced = records.iter_mut().find(|item| item.serial_hex == serial_hex);
        let Some(slot) = replaced else {
            return Err(pki_error("certificate serial was not issued by this role"));
        };
        *slot = record;

        let number = next_crl_number(&self.data_dir, role)?;
        let issuer = load_issuer(&self.data_dir, role.as_str(), ca_password)?;
        let now = OffsetDateTime::now_utc();
        let revoked_certs = records
            .iter()
            .filter(|item| item.revoked)
            .map(revoked_certificate)
            .collect::<Result<Vec<_>, GatewayError>>()?;
        let crl = CertificateRevocationListParams {
            this_update: now,
            next_update: now + Duration::days(7),
            crl_number: SerialNumber::from(number),
            issuing_distribution_point: None,
            revoked_certs,
            key_identifier_method: KeyIdMethod::Sha256,
        }
        .signed_by(&issuer)
        .map_err(|_| pki_error("could not sign certificate revocation list"))?;
        let pem = crl.pem().map_err(|_| pki_error("could not encode certificate revocation list"))?;

        // A committed record makes every partial state recoverable by retrying the same revocation.
        let record = records
            .iter()
            .find(|item| item.serial_hex == serial_hex)
            .ok_or_else(|| pki_error("certificate serial was not issued by this role"))?;
        write_issued_record(&self.data_dir, role, record)?;
        // Reserve the number before publishing the CRL so a crash can create a gap but never reuse it.
        atomic_write(&crl_number_path(&self.data_dir, role), number.to_string().as_bytes(), 0o644)?;
        atomic_write(&crl_path(&self.data_dir, role), pem.as_bytes(), 0o644)?;
        Ok(GeneratedCrl { number, pem })
    }
}

pub(crate) fn record_issued_certificate(
    data_dir: &Path,
    role: CertificateRole,
    serial_hex: &str,
    certificate_pem: &str,
    identity: &str,
) -> Result<(), GatewayError> {
    let _role_lock = lock_role(data_dir, role)?;
    let record = IssuedRecord {
        serial_hex: normalize_serial_hex(serial_hex)?,
        certificate_pem: certificate_pem.to_string(),
        identity: identity.to_string(),
        role: role.as_str().to_string(),
        revoked: false,
        revoked_at: None,
        reason: None,
    };
    let path = issued_record_path(data_dir, role, &record.serial_hex);
    if fs::symlink_metadata(&path).is_ok() {
        return Err(pki_error("certificate serial collision"));
    }
    write_issued_record_new(data_dir, role, &record)
}

pub fn write_output_file(path: &Path, contents: &[u8], mode: u32) -> Result<(), GatewayError> {
    atomic_write_new(path, contents, mode)
}

fn reject_existing_pki(data_dir: &Path) -> Result<(), GatewayError> {
    if !data_dir.exists() {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(data_dir).map_err(|_| pki_error("could not inspect PKI data directory"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(pki_error("PKI data directory must be a real directory"));
    }
    let mut entries = fs::read_dir(data_dir).map_err(|_| pki_error("could not inspect PKI data directory"))?;
    if entries.next().transpose().map_err(|_| pki_error("could not inspect PKI data directory"))?.is_some() {
        return Err(pki_error("PKI data directory already exists"));
    }
    Ok(())
}

fn lock_initialization(data_dir: &Path) -> Result<File, GatewayError> {
    let parent = data_dir.parent().ok_or_else(|| pki_error("PKI data directory must have a parent"))?;
    let name = data_dir
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| pki_error("invalid PKI data directory name"))?;
    let path = parent.join(format!(".{name}.init.lock"));
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&path).map_err(|_| pki_error("could not open PKI initialization lock"))?;
    let path_metadata = validate_regular_file(&path, Some(0o600), "PKI initialization lock")?;
    let opened_metadata = file.metadata().map_err(|_| pki_error("could not inspect PKI initialization lock"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if path_metadata.dev() != opened_metadata.dev() || path_metadata.ino() != opened_metadata.ino() {
            return Err(pki_error("PKI initialization lock changed while opening"));
        }
    }
    file.lock().map_err(|_| pki_error("could not lock PKI initialization"))?;
    Ok(file)
}

fn create_initialization_dir(data_dir: &Path) -> Result<PathBuf, GatewayError> {
    let parent = data_dir.parent().ok_or_else(|| pki_error("PKI data directory must have a parent"))?;
    let name = data_dir
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| pki_error("invalid PKI data directory name"))?;
    let timestamp =
        SystemTime::now().duration_since(UNIX_EPOCH).map_err(|_| pki_error("could not initialize PKI"))?.as_nanos();
    for _ in 0..128 {
        let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(".{name}.init-{}-{timestamp}-{sequence}", std::process::id()));
        match fs::create_dir(&candidate) {
            Ok(()) => {
                create_secure_dir(&candidate)?;
                return Ok(candidate);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(pki_error("could not create PKI initialization directory")),
        }
    }
    Err(pki_error("could not allocate PKI initialization directory"))
}

fn publish_initialized_pki(staging_dir: &Path, data_dir: &Path) -> Result<(), GatewayError> {
    if fs::symlink_metadata(data_dir).is_ok() {
        reject_existing_pki(data_dir)?;
        fs::remove_dir(data_dir).map_err(|_| pki_error("could not replace empty PKI data directory"))?;
    }
    fs::rename(staging_dir, data_dir).map_err(|_| pki_error("could not publish initialized PKI"))?;
    #[cfg(unix)]
    {
        let parent = data_dir.parent().ok_or_else(|| pki_error("PKI data directory must have a parent"))?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| pki_error("could not persist initialized PKI"))?;
    }
    Ok(())
}

fn validate_existing_pki(data_dir: &Path) -> Result<(), GatewayError> {
    validate_directory(data_dir, "PKI data directory")?;
    for role in
        [ROOT, CertificateRole::Server.as_str(), CertificateRole::Edge.as_str(), CertificateRole::Client.as_str()]
    {
        validate_existing_role(data_dir, role)?;
    }
    Ok(())
}

fn validate_existing_role(data_dir: &Path, role: &str) -> Result<(), GatewayError> {
    let role_dir = data_dir.join(role);
    validate_directory(&role_dir, "PKI role directory")?;
    validate_regular_file(&certificate_path(data_dir, role), None, "CA certificate")?;
    validate_regular_file(&private_key_path(data_dir, role), Some(0o600), "CA private key")?;
    let issued = role_dir.join("issued");
    if fs::symlink_metadata(&issued).is_ok() {
        validate_directory(&issued, "issued certificate directory")?;
    }
    Ok(())
}

fn validate_directory(path: &Path, label: &str) -> Result<(), GatewayError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| pki_error(&format!("{label} is missing")))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(pki_error(&format!("{label} must be a real directory")));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o7777 != 0o700 {
            return Err(pki_error(&format!("{label} permissions must be 0700")));
        }
    }
    Ok(())
}

fn validate_regular_file(path: &Path, expected_mode: Option<u32>, label: &str) -> Result<fs::Metadata, GatewayError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| pki_error(&format!("{label} is missing")))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(pki_error(&format!("{label} must be a real regular file")));
    }
    #[cfg(unix)]
    if let Some(expected_mode) = expected_mode {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o7777 != expected_mode {
            return Err(pki_error(&format!("{label} permissions must be {expected_mode:04o}")));
        }
    }
    Ok(metadata)
}

fn read_regular_file(path: &Path, expected_mode: Option<u32>, label: &str) -> Result<Vec<u8>, GatewayError> {
    let path_metadata = validate_regular_file(path, expected_mode, label)?;
    let mut file = File::open(path).map_err(|_| pki_error(&format!("could not read {label}")))?;
    let opened_metadata = file.metadata().map_err(|_| pki_error(&format!("could not inspect {label}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if path_metadata.dev() != opened_metadata.dev() || path_metadata.ino() != opened_metadata.ino() {
            return Err(pki_error(&format!("{label} changed while opening")));
        }
    }
    let mut contents = Vec::new();
    file.read_to_end(&mut contents).map_err(|_| pki_error(&format!("could not read {label}")))?;
    Ok(contents)
}

fn lock_role(data_dir: &Path, role: CertificateRole) -> Result<File, GatewayError> {
    let path = data_dir.join(role.as_str()).join(".state.lock");
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&path).map_err(|_| pki_error("could not open PKI role lock"))?;
    let path_metadata = validate_regular_file(&path, Some(0o600), "PKI role lock")?;
    let opened_metadata = file.metadata().map_err(|_| pki_error("could not inspect PKI role lock"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if path_metadata.dev() != opened_metadata.dev() || path_metadata.ino() != opened_metadata.ino() {
            return Err(pki_error("PKI role lock changed while opening"));
        }
    }
    file.lock().map_err(|_| pki_error("could not lock PKI role state"))?;
    Ok(file)
}

fn ca_params(common_name: &str, constraint: BasicConstraints) -> CertificateParams {
    let now = OffsetDateTime::now_utc();
    let mut params = CertificateParams::default();
    params.distinguished_name.push(DnType::CommonName, common_name);
    params.is_ca = IsCa::Ca(constraint);
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature, KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params.not_before = now - Duration::days(1);
    params.not_after = now + Duration::days(3650);
    params
}

fn write_ca(
    data_dir: &Path,
    role: &str,
    certificate_pem: &str,
    key: &KeyPair,
    password: &Zeroizing<String>,
) -> Result<(), GatewayError> {
    let role_dir = data_dir.join(role);
    create_secure_dir(&role_dir)?;
    atomic_write(&certificate_path(data_dir, role), certificate_pem.as_bytes(), 0o644)?;

    let mut key_der = Zeroizing::new(key.serialize_der());
    let key_info =
        PrivateKeyInfoRef::from_der(key_der.as_slice()).map_err(|_| pki_error("could not encode CA private key"))?;
    let mut rng = p12_keystore::rand::rng();
    let encrypted =
        key_info.encrypt(&mut rng, password.as_bytes()).map_err(|_| pki_error("could not encrypt CA private key"))?;
    let pem = encrypted
        .to_pem("ENCRYPTED PRIVATE KEY", LineEnding::LF)
        .map_err(|_| pki_error("could not encode encrypted CA private key"))?;
    atomic_write(&private_key_path(data_dir, role), pem.as_bytes(), 0o600)?;
    key_der.zeroize();
    Ok(())
}

fn certificate_path(data_dir: &Path, role: &str) -> PathBuf {
    data_dir.join(role).join("ca.crt.pem")
}

fn private_key_path(data_dir: &Path, role: &str) -> PathBuf {
    data_dir.join(role).join("ca.key.encrypted.pem")
}

fn issued_dir(data_dir: &Path, role: CertificateRole) -> PathBuf {
    data_dir.join(role.as_str()).join("issued")
}

fn issued_record_path(data_dir: &Path, role: CertificateRole, serial_hex: &str) -> PathBuf {
    issued_dir(data_dir, role).join(format!("{serial_hex}.toml"))
}

fn crl_path(data_dir: &Path, role: CertificateRole) -> PathBuf {
    data_dir.join(role.as_str()).join("crl.pem")
}

fn crl_number_path(data_dir: &Path, role: CertificateRole) -> PathBuf {
    data_dir.join(role.as_str()).join("crl-number")
}

pub(crate) fn load_issuer(
    data_dir: &Path,
    role: &str,
    password: &Zeroizing<String>,
) -> Result<Issuer<'static, KeyPair>, GatewayError> {
    use pkcs8::EncryptedPrivateKeyInfoRef;
    use x509_parser::pem::parse_x509_pem;

    require_nonempty_secret(password, "CA password")?;
    let encrypted_pem = read_regular_file(&private_key_path(data_dir, role), Some(0o600), "CA private key")?;
    let (_, pem) = parse_x509_pem(&encrypted_pem).map_err(|_| pki_error("could not parse CA private key"))?;
    let encrypted = EncryptedPrivateKeyInfoRef::try_from(pem.contents.as_slice())
        .map_err(|_| pki_error("could not parse CA private key"))?;
    let decrypted = encrypted.decrypt(password.as_bytes()).map_err(|_| pki_error("CA password was rejected"))?;
    let key = KeyPair::try_from(decrypted.as_bytes()).map_err(|_| pki_error("could not load CA private key"))?;
    let certificate_pem =
        String::from_utf8(read_regular_file(&certificate_path(data_dir, role), None, "CA certificate")?)
            .map_err(|_| pki_error("could not parse CA certificate"))?;
    Issuer::from_ca_cert_pem(&certificate_pem, key).map_err(|_| pki_error("could not load CA certificate"))
}

pub(crate) fn read_certificate(data_dir: &Path, role: &str) -> Result<String, GatewayError> {
    String::from_utf8(read_regular_file(&certificate_path(data_dir, role), None, "CA certificate")?)
        .map_err(|_| pki_error("could not parse CA certificate"))
}

pub(crate) fn read_optional_certificate(data_dir: &Path, role: &str) -> Result<Option<String>, GatewayError> {
    match fs::symlink_metadata(certificate_path(data_dir, role)) {
        Ok(_) => read_certificate(data_dir, role).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(pki_error("CA certificate could not be inspected")),
    }
}

fn read_issued_record(data_dir: &Path, role: CertificateRole, serial_hex: &str) -> Result<IssuedRecord, GatewayError> {
    let path = issued_record_path(data_dir, role, serial_hex);
    let input = String::from_utf8(
        read_regular_file(&path, None, "issued certificate record")
            .map_err(|_| pki_error("certificate serial was not issued by this role"))?,
    )
    .map_err(|_| pki_error("invalid issued certificate record"))?;
    parse_issued_record(&input, role, serial_hex)
}

fn read_issued_records(data_dir: &Path, role: CertificateRole) -> Result<Vec<IssuedRecord>, GatewayError> {
    let directory = issued_dir(data_dir, role);
    let entries = fs::read_dir(&directory).map_err(|_| pki_error("could not read issued certificate records"))?;
    entries
        .map(|entry| {
            let entry = entry.map_err(|_| pki_error("could not read issued certificate records"))?;
            if !entry.file_type().map_err(|_| pki_error("could not read issued certificate records"))?.is_file() {
                return Err(pki_error("invalid issued certificate record"));
            }
            let input = String::from_utf8(read_regular_file(&entry.path(), None, "issued certificate record")?)
                .map_err(|_| pki_error("invalid issued certificate record"))?;
            let record: IssuedRecord =
                toml::from_str(&input).map_err(|_| pki_error("invalid issued certificate record"))?;
            let serial_hex = normalize_serial_hex(&record.serial_hex)?;
            if record.role != role.as_str() || record.serial_hex != serial_hex {
                return Err(pki_error("invalid issued certificate record"));
            }
            Ok(record)
        })
        .collect()
}

fn parse_issued_record(input: &str, role: CertificateRole, serial_hex: &str) -> Result<IssuedRecord, GatewayError> {
    let record: IssuedRecord = toml::from_str(input).map_err(|_| pki_error("invalid issued certificate record"))?;
    if record.role != role.as_str() || record.serial_hex != serial_hex {
        return Err(pki_error("invalid issued certificate record"));
    }
    Ok(record)
}

fn write_issued_record(data_dir: &Path, role: CertificateRole, record: &IssuedRecord) -> Result<(), GatewayError> {
    let directory = issued_dir(data_dir, role);
    create_secure_dir(&directory)?;
    let value = toml::to_string(record).map_err(|_| pki_error("could not encode issued certificate record"))?;
    atomic_write(&issued_record_path(data_dir, role, &record.serial_hex), value.as_bytes(), 0o644)
}

fn write_issued_record_new(data_dir: &Path, role: CertificateRole, record: &IssuedRecord) -> Result<(), GatewayError> {
    let directory = issued_dir(data_dir, role);
    create_secure_dir(&directory)?;
    let value = toml::to_string(record).map_err(|_| pki_error("could not encode issued certificate record"))?;
    atomic_write_new(&issued_record_path(data_dir, role, &record.serial_hex), value.as_bytes(), 0o644)
}

fn next_crl_number(data_dir: &Path, role: CertificateRole) -> Result<u64, GatewayError> {
    let path = crl_number_path(data_dir, role);
    let current = match fs::symlink_metadata(&path) {
        Ok(_) => {
            let value = String::from_utf8(read_regular_file(&path, None, "CRL number state")?)
                .map_err(|_| pki_error("invalid CRL number state"))?;
            value.trim().parse::<u64>().map_err(|_| pki_error("invalid CRL number state"))?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(_) => return Err(pki_error("could not inspect CRL number state")),
    };
    current.checked_add(1).ok_or_else(|| pki_error("CRL number is exhausted"))
}

fn revoked_certificate(record: &IssuedRecord) -> Result<RevokedCertParams, GatewayError> {
    let serial = SerialNumber::from(
        hex::decode(&record.serial_hex).map_err(|_| pki_error("invalid issued certificate record"))?,
    );
    let revoked_at = record.revoked_at.ok_or_else(|| pki_error("invalid issued certificate record"))?;
    let reason = record
        .reason
        .as_deref()
        .ok_or_else(|| pki_error("invalid issued certificate record"))?
        .parse::<RevocationReason>()?;
    Ok(RevokedCertParams {
        serial_number: serial,
        revocation_time: OffsetDateTime::from_unix_timestamp(revoked_at)
            .map_err(|_| pki_error("invalid issued certificate record"))?,
        reason_code: Some(match reason {
            RevocationReason::Unspecified => RcgenRevocationReason::Unspecified,
            RevocationReason::KeyCompromise => RcgenRevocationReason::KeyCompromise,
            RevocationReason::CaCompromise => RcgenRevocationReason::CaCompromise,
            RevocationReason::AffiliationChanged => RcgenRevocationReason::AffiliationChanged,
            RevocationReason::Superseded => RcgenRevocationReason::Superseded,
            RevocationReason::CessationOfOperation => RcgenRevocationReason::CessationOfOperation,
            RevocationReason::CertificateHold => RcgenRevocationReason::CertificateHold,
            RevocationReason::PrivilegeWithdrawn => RcgenRevocationReason::PrivilegeWithdrawn,
            RevocationReason::AaCompromise => RcgenRevocationReason::AaCompromise,
        }),
        invalidity_date: None,
    })
}

fn normalize_serial_hex(serial_hex: &str) -> Result<String, GatewayError> {
    if serial_hex.is_empty() || !serial_hex.len().is_multiple_of(2) || serial_hex.len() > MAX_SERIAL_BYTES * 2 {
        return Err(pki_error("invalid certificate serial"));
    }
    let decoded = hex::decode(serial_hex).map_err(|_| pki_error("invalid certificate serial"))?;
    if decoded.is_empty() || decoded.len() > MAX_SERIAL_BYTES {
        return Err(pki_error("invalid certificate serial"));
    }
    Ok(hex::encode(decoded))
}

fn require_nonempty_secret(secret: &Zeroizing<String>, name: &str) -> Result<(), GatewayError> {
    if secret.is_empty() {
        return Err(pki_error(&format!("{name} must not be empty")));
    }
    Ok(())
}

fn create_secure_dir(path: &Path) -> Result<(), GatewayError> {
    fs::create_dir_all(path).map_err(|_| pki_error("could not create PKI data directory"))?;
    let metadata = fs::symlink_metadata(path).map_err(|_| pki_error("could not inspect PKI data directory"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(pki_error("PKI data directory must be a real directory"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|_| pki_error("could not secure PKI data directory"))?;
    }
    Ok(())
}

pub(crate) fn atomic_write(path: &Path, contents: &[u8], mode: u32) -> Result<(), GatewayError> {
    atomic_write_impl(path, contents, mode, true)
}

fn atomic_write_new(path: &Path, contents: &[u8], mode: u32) -> Result<(), GatewayError> {
    atomic_write_impl(path, contents, mode, false)
}

fn atomic_write_impl(path: &Path, contents: &[u8], mode: u32, replace: bool) -> Result<(), GatewayError> {
    let parent = path.parent().ok_or_else(|| pki_error("invalid PKI state path"))?;
    validate_directory(parent, "PKI state directory")?;
    let file_name =
        path.file_name().and_then(|value| value.to_str()).ok_or_else(|| pki_error("invalid PKI state path"))?;
    let timestamp =
        SystemTime::now().duration_since(UNIX_EPOCH).map_err(|_| pki_error("could not write PKI state"))?.as_nanos();
    let mut temporary = None;
    for _ in 0..128 {
        let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(".{file_name}.tmp-{}-{timestamp}-{sequence}", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(mode);
        }
        match options.open(&candidate) {
            Ok(file) => {
                temporary = Some((candidate, file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(pki_error("could not write PKI state")),
        }
    }
    let Some((temporary_path, mut file)) = temporary else {
        return Err(pki_error("could not allocate PKI state file"));
    };

    let result = (|| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(mode))
                .map_err(|_| pki_error("could not secure PKI state"))?;
        }
        file.write_all(contents).map_err(|_| pki_error("could not write PKI state"))?;
        file.sync_all().map_err(|_| pki_error("could not persist PKI state"))?;
        drop(file);
        if replace {
            fs::rename(&temporary_path, path).map_err(|_| pki_error("could not commit PKI state"))?;
        } else {
            fs::hard_link(&temporary_path, path).map_err(|_| pki_error("output file already exists"))?;
            fs::remove_file(&temporary_path).map_err(|_| pki_error("could not finalize output file"))?;
        }
        #[cfg(unix)]
        {
            fs::File::open(parent)
                .and_then(|directory| directory.sync_all())
                .map_err(|_| pki_error("could not persist PKI state"))?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result
}

pub(crate) fn pki_error(message: &str) -> GatewayError {
    GatewayError { code: GatewayErrorCode::Internal, message: message.to_string() }
}
