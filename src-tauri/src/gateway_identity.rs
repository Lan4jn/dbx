use std::path::Path;

use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use aes_gcm::{Aes256Gcm, Key};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use dbx_core::db::dbx_gateway::{GatewayClientIdentity, GatewayIdentityMetadata, GatewayIdentityProvider};
use dbx_core::storage::{GatewayIdentityEncryptedRecord, GatewayIdentityKeyState, Storage};
use log::warn;
use p12_keystore::KeyStore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use x509_parser::parse_x509_certificate;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

const KEYRING_SERVICE: &str = "fun.dbx.gateway";
const MASTER_KEY_ACCOUNT: &str = "master-key-v1";
const IDENTITY_SECRET_VERSION: u8 = 1;
const IDENTITY_KEY_VERSION: u32 = 1;
const MASTER_KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;
const AAD_PREFIX: &[u8] = b"DBX-GATEWAY-IDENTITY";

#[derive(Clone)]
pub struct StorageGatewayIdentityProvider {
    storage: Storage,
}

#[derive(Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
struct StoredIdentity {
    version: u8,
    certificate_chain_der: Vec<String>,
    private_key_pkcs8_der: String,
}

impl StoredIdentity {
    fn from_identity(identity: &GatewayClientIdentity) -> Self {
        Self {
            version: IDENTITY_SECRET_VERSION,
            certificate_chain_der: identity.certificate_chain_der.iter().map(|value| STANDARD.encode(value)).collect(),
            private_key_pkcs8_der: STANDARD.encode(&identity.private_key_pkcs8_der),
        }
    }

    fn into_identity(self) -> Result<GatewayClientIdentity, String> {
        if self.version != IDENTITY_SECRET_VERSION {
            return Err("Unsupported DBX Gateway identity format".to_string());
        }
        let certificate_chain_der = self
            .certificate_chain_der
            .iter()
            .map(|value| STANDARD.decode(value).map_err(|_| "Invalid Gateway certificate data".to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        let private_key_pkcs8_der =
            STANDARD.decode(&self.private_key_pkcs8_der).map_err(|_| "Invalid Gateway private key data".to_string())?;
        if certificate_chain_der.is_empty() || private_key_pkcs8_der.is_empty() {
            return Err("Gateway identity is incomplete".to_string());
        }
        Ok(GatewayClientIdentity { certificate_chain_der, private_key_pkcs8_der })
    }
}

impl StorageGatewayIdentityProvider {
    pub fn new(storage: Storage) -> Self {
        Self { storage }
    }

    pub async fn import_pkcs12(
        &self,
        path: &Path,
        password: &str,
        name: &str,
    ) -> Result<GatewayIdentityMetadata, String> {
        if password.is_empty() {
            return Err("PKCS#12 password is required".to_string());
        }
        let data = Zeroizing::new(std::fs::read(path).map_err(|_| "Could not read the PKCS#12 file".to_string())?);
        let id = uuid::Uuid::new_v4().to_string();
        let password = Zeroizing::new(password.to_string());
        let name = name.to_string();
        let (identity, metadata) = tokio::task::spawn_blocking(move || parse_pkcs12(&data, &password, &name, id))
            .await
            .map_err(|_| "Gateway identity import task failed".to_string())??;
        let record_identity_id = metadata.id.clone();
        self.storage
            .save_gateway_identity_bundle_with_key_initialization(&metadata, move |state| {
                let master_key = master_key_for_state(state)?;
                encrypt_identity(&record_identity_id, &identity, &master_key)
            })
            .await?;
        Ok(metadata)
    }

    pub async fn delete(&self, identity_id: &str) -> Result<(), String> {
        self.storage.delete_gateway_identity_bundle(identity_id).await?;
        let identity_id = identity_id.to_string();
        let cleanup_result =
            tokio::task::spawn_blocking(move || legacy_entry(&identity_id)?.delete_credential().map_err(keyring_error))
                .await;
        let cleanup_failed = match cleanup_result {
            Ok(Ok(())) => false,
            Ok(Err(error)) if error.contains("not found") => false,
            _ => true,
        };
        if cleanup_failed {
            warn!("Could not remove a legacy DBX Gateway credential after deleting its database record");
        }
        Ok(())
    }

    async fn load_current(&self, identity_id: &str) -> Result<Option<GatewayClientIdentity>, String> {
        let Some(record) = self.storage.load_gateway_identity_record(identity_id).await? else {
            return Ok(None);
        };
        let master_key = load_master_key().await?;
        decrypt_identity(identity_id, &record, &master_key).map(Some)
    }

    async fn migrate_legacy(&self, identity_id: &str) -> Result<GatewayClientIdentity, String> {
        let migration_id = identity_id.to_string();
        let migrated = self
            .storage
            .migrate_gateway_identity_bundle_with_key_initialization(identity_id, move |metadata, state| {
                let identity = load_legacy_identity_blocking(&migration_id)?;
                let master_key = master_key_for_state(state)?;
                let record = encrypt_identity(&metadata.id, &identity, &master_key)?;
                Ok((record, identity))
            })
            .await?;
        let migrated = match migrated {
            Some(identity) => identity,
            None => self
                .load_current(identity_id)
                .await?
                .ok_or_else(|| "Gateway identity migration did not produce an identity".to_string())?,
        };
        let cleanup_id = identity_id.to_string();
        let cleanup_result =
            tokio::task::spawn_blocking(move || legacy_entry(&cleanup_id)?.delete_credential().map_err(keyring_error))
                .await;
        let cleanup_failed = match cleanup_result {
            Ok(Ok(())) => false,
            Ok(Err(error)) if error.contains("not found") => false,
            _ => true,
        };
        if cleanup_failed {
            warn!("Could not remove a legacy DBX Gateway credential after migration");
        }
        Ok(migrated)
    }
}

fn master_key_for_state(state: Option<GatewayIdentityKeyState>) -> Result<Zeroizing<[u8; MASTER_KEY_LEN]>, String> {
    let key = if state.is_some() { load_master_key_blocking()? } else { load_or_create_master_key_blocking()? };
    if let Some(state) = state {
        if state.key_version != IDENTITY_KEY_VERSION || state.key_fingerprint_sha256 != master_key_fingerprint(&key) {
            return Err("Gateway identity master key does not match this database".to_string());
        }
    }
    Ok(key)
}

fn encrypt_identity(
    identity_id: &str,
    identity: &GatewayClientIdentity,
    master_key: &[u8; MASTER_KEY_LEN],
) -> Result<GatewayIdentityEncryptedRecord, String> {
    let plaintext = Zeroizing::new(
        serde_json::to_vec(&StoredIdentity::from_identity(identity))
            .map_err(|_| "Could not encode Gateway identity".to_string())?,
    );
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(master_key));
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ciphertext = cipher
        .encrypt(&nonce, Payload { msg: &plaintext, aad: &identity_aad(identity_id, IDENTITY_SECRET_VERSION) })
        .map_err(|_| "Could not encrypt Gateway identity".to_string())?;
    Ok(GatewayIdentityEncryptedRecord {
        identity_id: identity_id.to_string(),
        format_version: IDENTITY_SECRET_VERSION,
        key_version: IDENTITY_KEY_VERSION,
        key_fingerprint_sha256: master_key_fingerprint(master_key),
        nonce: nonce.to_vec(),
        ciphertext,
    })
}

fn decrypt_identity(
    identity_id: &str,
    record: &GatewayIdentityEncryptedRecord,
    master_key: &[u8; MASTER_KEY_LEN],
) -> Result<GatewayClientIdentity, String> {
    if record.identity_id != identity_id {
        return Err("Gateway identity record does not match the requested identity".to_string());
    }
    if record.format_version != IDENTITY_SECRET_VERSION {
        return Err("Unsupported DBX Gateway identity format".to_string());
    }
    if record.key_version != IDENTITY_KEY_VERSION {
        return Err("Unsupported DBX Gateway identity key version".to_string());
    }
    if record.key_fingerprint_sha256 != master_key_fingerprint(master_key) {
        return Err("Gateway identity master key does not match this database".to_string());
    }
    if record.nonce.len() != NONCE_LEN {
        return Err("Gateway identity nonce is damaged".to_string());
    }
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(master_key));
    let plaintext = Zeroizing::new(
        cipher
            .decrypt(
                aes_gcm::Nonce::from_slice(&record.nonce),
                Payload { msg: &record.ciphertext, aad: &identity_aad(identity_id, record.format_version) },
            )
            .map_err(|_| "Gateway identity ciphertext is damaged or cannot be decrypted".to_string())?,
    );
    let stored: StoredIdentity =
        serde_json::from_slice(&plaintext).map_err(|_| "Invalid Gateway identity data".to_string())?;
    stored.into_identity()
}

fn identity_aad(identity_id: &str, format_version: u8) -> Vec<u8> {
    let mut aad = Vec::with_capacity(AAD_PREFIX.len() + identity_id.len() + 4);
    aad.extend_from_slice(AAD_PREFIX);
    aad.push(0);
    aad.extend_from_slice(format_version.to_string().as_bytes());
    aad.push(0);
    aad.extend_from_slice(identity_id.as_bytes());
    aad
}

fn master_key_fingerprint(master_key: &[u8; MASTER_KEY_LEN]) -> String {
    hex::encode(Sha256::digest(master_key))
}

async fn load_master_key() -> Result<Zeroizing<[u8; MASTER_KEY_LEN]>, String> {
    tokio::task::spawn_blocking(load_master_key_blocking)
        .await
        .map_err(|_| "Gateway identity master key task failed".to_string())?
}

fn load_master_key_blocking() -> Result<Zeroizing<[u8; MASTER_KEY_LEN]>, String> {
    let secret = Zeroizing::new(master_key_entry()?.get_secret().map_err(master_key_error)?);
    decode_master_key(secret)
}

fn load_or_create_master_key_blocking() -> Result<Zeroizing<[u8; MASTER_KEY_LEN]>, String> {
    match master_key_entry()?.get_secret() {
        Ok(secret) => decode_master_key(Zeroizing::new(secret)),
        Err(keyring::Error::NoEntry) => create_master_key_with(
            |key| master_key_entry()?.set_secret(key).map_err(master_key_error),
            load_master_key_blocking,
        ),
        Err(error) => Err(master_key_error(error)),
    }
}

fn create_master_key_with(
    persist: impl FnOnce(&[u8]) -> Result<(), String>,
    reload: impl FnOnce() -> Result<Zeroizing<[u8; MASTER_KEY_LEN]>, String>,
) -> Result<Zeroizing<[u8; MASTER_KEY_LEN]>, String> {
    let mut generated = Aes256Gcm::generate_key(&mut OsRng);
    let mut key = Zeroizing::new([0u8; MASTER_KEY_LEN]);
    key.copy_from_slice(generated.as_slice());
    generated.zeroize();
    persist(key.as_ref())?;
    reload()
}

fn decode_master_key(secret: Zeroizing<Vec<u8>>) -> Result<Zeroizing<[u8; MASTER_KEY_LEN]>, String> {
    <[u8; MASTER_KEY_LEN]>::try_from(secret.as_slice()).map(Zeroizing::new).map_err(|_| {
        "Gateway identity master key is damaged; encrypted Gateway identities cannot be decrypted".to_string()
    })
}

fn master_key_entry() -> Result<keyring::Entry, String> {
    keyring::Entry::new(KEYRING_SERVICE, MASTER_KEY_ACCOUNT).map_err(master_key_error)
}

fn legacy_entry(identity_id: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(KEYRING_SERVICE, identity_id).map_err(keyring_error)
}

fn load_legacy_identity_blocking(identity_id: &str) -> Result<GatewayClientIdentity, String> {
    let secret = Zeroizing::new(legacy_entry(identity_id)?.get_password().map_err(keyring_error)?);
    let json =
        Zeroizing::new(STANDARD.decode(secret.as_bytes()).map_err(|_| "Invalid Gateway identity data".to_string())?);
    let stored: StoredIdentity =
        serde_json::from_slice(&json).map_err(|_| "Invalid Gateway identity data".to_string())?;
    stored.into_identity()
}

fn parse_pkcs12(
    data: &[u8],
    password: &str,
    name: &str,
    id: String,
) -> Result<(GatewayClientIdentity, GatewayIdentityMetadata), String> {
    let keystore =
        KeyStore::from_pkcs12(data, password).map_err(|_| "Could not unlock or parse the PKCS#12 file".to_string())?;
    let (_, chain) = keystore
        .private_key_chain()
        .ok_or_else(|| "PKCS#12 does not contain a private key and certificate chain".to_string())?;
    if chain.chain().is_empty() {
        return Err("PKCS#12 certificate chain is empty".to_string());
    }

    let identity = GatewayClientIdentity {
        certificate_chain_der: chain.chain().iter().map(|certificate| certificate.as_der().to_vec()).collect(),
        private_key_pkcs8_der: chain.key().to_vec(),
    };
    let leaf = &identity.certificate_chain_der[0];
    let (_, certificate) =
        parse_x509_certificate(leaf).map_err(|_| "PKCS#12 leaf certificate is invalid".to_string())?;
    let expires_at = chrono::DateTime::<chrono::Utc>::from_timestamp(certificate.validity().not_after.timestamp(), 0)
        .ok_or_else(|| "PKCS#12 certificate expiry is invalid".to_string())?
        .to_rfc3339();
    let fingerprint_sha256 = hex::encode(Sha256::digest(leaf));
    let metadata = GatewayIdentityMetadata {
        id,
        name: name.trim().to_string(),
        subject: certificate.subject().to_string(),
        expires_at,
        fingerprint_sha256,
    };
    Ok((identity, metadata))
}

#[async_trait::async_trait]
impl GatewayIdentityProvider for StorageGatewayIdentityProvider {
    async fn load(&self, identity_id: &str) -> Result<GatewayClientIdentity, String> {
        if let Some(identity) = self.load_current(identity_id).await? {
            return Ok(identity);
        }
        self.migrate_legacy(identity_id).await
    }
}

fn keyring_error(error: keyring::Error) -> String {
    match error {
        keyring::Error::NoEntry => {
            "Gateway identity was not found; re-import the Gateway client certificate".to_string()
        }
        keyring::Error::TooLong(_, _) => {
            "Gateway identity is too large for the operating system credential store; update DBX and re-import it"
                .to_string()
        }
        _ => format!("The operating system credential store is unavailable or locked: {error}"),
    }
}

fn master_key_error(error: keyring::Error) -> String {
    match error {
        keyring::Error::NoEntry => {
            "The current user's DBX Gateway master key is missing; encrypted Gateway identities in this database cannot be decrypted"
                .to_string()
        }
        _ => format!("Could not access the operating system DBX Gateway master key: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use dbx_core::db::dbx_gateway::GatewayClientIdentity;
    #[cfg(not(windows))]
    use dbx_gateway::pki::{ClientIssueRequest, PkiStore};
    #[cfg(not(windows))]
    use zeroize::Zeroizing;

    #[cfg(not(windows))]
    use super::parse_pkcs12;
    use super::{decrypt_identity, encrypt_identity, keyring_error, master_key_error, StoredIdentity};

    fn sample_identity() -> GatewayClientIdentity {
        GatewayClientIdentity {
            certificate_chain_der: vec![vec![1, 2, 3], vec![4, 5]],
            private_key_pkcs8_der: vec![6, 7, 8],
        }
    }

    #[test]
    fn identity_secret_round_trip_preserves_der() {
        let identity = sample_identity();

        let decoded = StoredIdentity::from_identity(&identity).into_identity().unwrap();
        assert_eq!(decoded.certificate_chain_der, identity.certificate_chain_der);
        assert_eq!(decoded.private_key_pkcs8_der, identity.private_key_pkcs8_der);
    }

    #[test]
    fn encrypted_identity_round_trip_preserves_der() {
        let key = [7u8; 32];
        let identity = sample_identity();

        let record = encrypt_identity("identity-1", &identity, &key).unwrap();
        let decoded = decrypt_identity("identity-1", &record, &key).unwrap();

        assert_eq!(decoded.certificate_chain_der, identity.certificate_chain_der);
        assert_eq!(decoded.private_key_pkcs8_der, identity.private_key_pkcs8_der);
    }

    #[test]
    fn encrypted_identity_uses_unique_nonce() {
        let key = [7u8; 32];
        let identity = sample_identity();

        let first = encrypt_identity("identity-1", &identity, &key).unwrap();
        let second = encrypt_identity("identity-1", &identity, &key).unwrap();

        assert_ne!(first.nonce, second.nonce);
        assert_ne!(first.ciphertext, second.ciphertext);
    }

    #[test]
    fn encrypted_identity_rejects_wrong_key() {
        let identity = sample_identity();
        let record = encrypt_identity("identity-1", &identity, &[7u8; 32]).unwrap();

        let err = match decrypt_identity("identity-1", &record, &[8u8; 32]) {
            Ok(_) => panic!("wrong key should be rejected"),
            Err(err) => err,
        };

        assert!(err.contains("master key"));
    }

    #[test]
    fn encrypted_identity_rejects_tampered_ciphertext() {
        let key = [7u8; 32];
        let identity = sample_identity();
        let mut record = encrypt_identity("identity-1", &identity, &key).unwrap();
        record.ciphertext[0] ^= 0x01;

        let err = match decrypt_identity("identity-1", &record, &key) {
            Ok(_) => panic!("tampered ciphertext should be rejected"),
            Err(err) => err,
        };

        assert!(err.contains("ciphertext"));
    }

    #[test]
    fn encrypted_identity_rejects_tampered_nonce() {
        let key = [7u8; 32];
        let identity = sample_identity();
        let mut record = encrypt_identity("identity-1", &identity, &key).unwrap();
        record.nonce[0] ^= 0x01;

        assert!(decrypt_identity("identity-1", &record, &key).is_err());
    }

    #[test]
    fn encrypted_identity_rejects_different_identity_id() {
        let key = [7u8; 32];
        let identity = sample_identity();
        let record = encrypt_identity("identity-1", &identity, &key).unwrap();

        assert!(decrypt_identity("identity-2", &record, &key).is_err());
    }

    #[test]
    fn encrypted_identity_rejects_unsupported_format_version() {
        let key = [7u8; 32];
        let identity = sample_identity();
        let mut record = encrypt_identity("identity-1", &identity, &key).unwrap();
        record.format_version += 1;

        assert!(decrypt_identity("identity-1", &record, &key).is_err());
    }

    #[test]
    fn keyring_too_long_error_is_actionable() {
        let err = keyring_error(keyring::Error::TooLong("credential".to_string(), 2560));

        assert!(err.contains("too large"));
        assert!(!err.contains("unavailable or locked"));
    }

    #[test]
    fn missing_master_key_is_not_reported_as_missing_identity() {
        let err = master_key_error(keyring::Error::NoEntry);

        assert!(err.contains("master key"));
        assert!(!err.contains("identity was not found"));
    }

    #[test]
    fn generated_master_key_requires_successful_keyring_reload() {
        let result = super::create_master_key_with(|_| Ok(()), || Err("simulated keyring reload failure".to_string()));

        assert_eq!(result.unwrap_err(), "simulated keyring reload failure");
    }

    #[test]
    #[cfg(not(windows))]
    fn identity_import_parses_a_gateway_pki_bundle() {
        let directory = tempfile::tempdir().unwrap();
        let ca_password = Zeroizing::new("ca-password".to_string());
        let bundle_password = Zeroizing::new("bundle-password".to_string());
        let store = PkiStore::init(directory.path(), &ca_password).unwrap();
        let bundle = store
            .issue_client(
                ClientIssueRequest {
                    client_id: "desktop-test",
                    validity: time::Duration::days(30),
                    bundle_password: &bundle_password,
                },
                &ca_password,
            )
            .unwrap();

        let (identity, metadata) =
            parse_pkcs12(&bundle.pkcs12_der, &bundle_password, "Desktop Test", "identity-test".to_string()).unwrap();
        assert!(!identity.private_key_pkcs8_der.is_empty());
        assert!(identity.certificate_chain_der.len() >= 2);
        assert_eq!(metadata.id, "identity-test");
        assert_eq!(metadata.name, "Desktop Test");
        assert!(metadata.subject.contains("desktop-test"));
        assert_eq!(metadata.fingerprint_sha256.len(), 64);
    }
}
