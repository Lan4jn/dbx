use std::path::Path;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use dbx_core::db::dbx_gateway::{GatewayClientIdentity, GatewayIdentityMetadata, GatewayIdentityProvider};
use p12_keystore::KeyStore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use x509_parser::parse_x509_certificate;

const KEYRING_SERVICE: &str = "fun.dbx.gateway";
const IDENTITY_SECRET_VERSION: u8 = 1;

#[derive(Clone, Default)]
pub struct KeyringGatewayIdentityProvider;

#[derive(Serialize, Deserialize)]
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
            .into_iter()
            .map(|value| STANDARD.decode(value).map_err(|_| "Invalid Gateway certificate data".to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        let private_key_pkcs8_der =
            STANDARD.decode(self.private_key_pkcs8_der).map_err(|_| "Invalid Gateway private key data".to_string())?;
        if certificate_chain_der.is_empty() || private_key_pkcs8_der.is_empty() {
            return Err("Gateway identity is incomplete".to_string());
        }
        Ok(GatewayClientIdentity { certificate_chain_der, private_key_pkcs8_der })
    }
}

impl KeyringGatewayIdentityProvider {
    pub fn import_pkcs12(&self, path: &Path, password: &str, name: &str) -> Result<GatewayIdentityMetadata, String> {
        if password.is_empty() {
            return Err("PKCS#12 password is required".to_string());
        }
        let data = std::fs::read(path).map_err(|_| "Could not read the PKCS#12 file".to_string())?;
        let id = uuid::Uuid::new_v4().to_string();
        let (identity, metadata) = parse_pkcs12(&data, password, name, id)?;
        self.store(&metadata.id, &identity)?;
        Ok(metadata)
    }

    fn store(&self, identity_id: &str, identity: &GatewayClientIdentity) -> Result<(), String> {
        let json = serde_json::to_vec(&StoredIdentity::from_identity(identity))
            .map_err(|_| "Could not encode Gateway identity".to_string())?;
        let secret = STANDARD.encode(json);
        keyring::Entry::new(KEYRING_SERVICE, identity_id)
            .map_err(keyring_error)?
            .set_password(&secret)
            .map_err(keyring_error)
    }

    fn load_blocking(&self, identity_id: &str) -> Result<GatewayClientIdentity, String> {
        let secret = keyring::Entry::new(KEYRING_SERVICE, identity_id)
            .map_err(keyring_error)?
            .get_password()
            .map_err(keyring_error)?;
        let json = STANDARD.decode(secret).map_err(|_| "Invalid Gateway identity data".to_string())?;
        let stored: StoredIdentity =
            serde_json::from_slice(&json).map_err(|_| "Invalid Gateway identity data".to_string())?;
        stored.into_identity()
    }

    pub fn delete(&self, identity_id: &str) -> Result<(), String> {
        keyring::Entry::new(KEYRING_SERVICE, identity_id)
            .map_err(keyring_error)?
            .delete_credential()
            .map_err(keyring_error)
    }
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
impl GatewayIdentityProvider for KeyringGatewayIdentityProvider {
    async fn load(&self, identity_id: &str) -> Result<GatewayClientIdentity, String> {
        let provider = self.clone();
        let identity_id = identity_id.to_string();
        tokio::task::spawn_blocking(move || provider.load_blocking(&identity_id))
            .await
            .map_err(|_| "Gateway identity task failed".to_string())?
    }
}

fn keyring_error(_error: keyring::Error) -> String {
    "The operating system credential store is unavailable or locked".to_string()
}

#[cfg(test)]
mod tests {
    use dbx_core::db::dbx_gateway::GatewayClientIdentity;
    use dbx_gateway::pki::{ClientIssueRequest, PkiStore};
    use zeroize::Zeroizing;

    use super::{parse_pkcs12, StoredIdentity};

    #[test]
    fn identity_secret_round_trip_preserves_der() {
        let identity = GatewayClientIdentity {
            certificate_chain_der: vec![vec![1, 2, 3], vec![4, 5]],
            private_key_pkcs8_der: vec![6, 7, 8],
        };

        let decoded = StoredIdentity::from_identity(&identity).into_identity().unwrap();
        assert_eq!(decoded.certificate_chain_der, identity.certificate_chain_der);
        assert_eq!(decoded.private_key_pkcs8_der, identity.private_key_pkcs8_der);
    }

    #[test]
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
