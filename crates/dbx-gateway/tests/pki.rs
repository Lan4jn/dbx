use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};

use std::net::{IpAddr, Ipv4Addr};

use dbx_gateway::pki::{
    write_output_file, CertificateRole, ClientIssueRequest, EdgeIssueRequest, PkiStore, RevocationReason,
    ServerIssueRequest,
};
use p12_keystore::KeyStore;
use pkcs8::der::Decode;
use pkcs8::{EncryptedPrivateKeyInfoRef, PrivateKeyInfoRef};
use rcgen::{CertificateParams, DnType, KeyPair, SanType};
use x509_parser::extensions::GeneralName;
use x509_parser::pem::parse_x509_pem;
use x509_parser::prelude::{parse_x509_certificate, parse_x509_crl};
use zeroize::Zeroizing;

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

fn temp_dir() -> std::path::PathBuf {
    let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "dbx-gateway-pki-{}-{}-{}",
        std::process::id(),
        time::OffsetDateTime::now_utc().unix_timestamp_nanos(),
        sequence
    ));
    fs::create_dir(&path).unwrap();
    path
}

#[test]
fn initializes_role_separated_offline_ca_store() {
    let data_dir = temp_dir();
    let password = Zeroizing::new("correct horse battery staple".to_string());

    let store = PkiStore::init(&data_dir, &password).unwrap();

    assert!(store.root_certificate_path().is_file());
    assert!(store.server_ca_certificate_path().is_file());
    assert!(store.edge_ca_certificate_path().is_file());
    assert!(store.client_ca_certificate_path().is_file());

    fs::remove_dir_all(data_dir).unwrap();
}

#[test]
fn online_edge_store_does_not_require_root_server_or_client_private_keys() {
    let offline = temp_dir();
    let online = temp_dir();
    let password = Zeroizing::new("ca-password".to_string());
    PkiStore::init(&offline, &password).unwrap();
    fs::create_dir(online.join("edge")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&online, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(online.join("edge"), fs::Permissions::from_mode(0o700)).unwrap();
    }
    fs::copy(offline.join("edge/ca.crt.pem"), online.join("edge/ca.crt.pem")).unwrap();
    fs::copy(offline.join("edge/ca.key.encrypted.pem"), online.join("edge/ca.key.encrypted.pem")).unwrap();

    let store = PkiStore::open_online_edge(&online).unwrap();
    assert!(PkiStore::open(&online).is_err());
    assert!(!online.join("root/ca.key.encrypted.pem").exists());
    assert!(!online.join("server/ca.key.encrypted.pem").exists());
    assert!(!online.join("client/ca.key.encrypted.pem").exists());

    let key = KeyPair::generate().unwrap();
    let csr = CertificateParams::default().serialize_request(&key).unwrap();
    let issued = store
        .issue_edge(
            EdgeIssueRequest {
                edge_id: "edge-online-01",
                csr_der: csr.der(),
                validity: time::Duration::days(30),
            },
            &password,
        )
        .unwrap();
    assert_eq!(issued.chain_pem, fs::read_to_string(online.join("edge/ca.crt.pem")).unwrap());

    fs::remove_dir_all(offline).unwrap();
    fs::remove_dir_all(online).unwrap();
}

#[test]
fn initialization_fails_closed_for_existing_pki_and_empty_password() {
    let data_dir = temp_dir();
    let password = Zeroizing::new("ca-password".to_string());
    let store = PkiStore::init(&data_dir, &password).unwrap();
    let root_before = fs::read(store.root_certificate_path()).unwrap();

    assert!(PkiStore::init(&data_dir, &password).is_err());
    assert_eq!(fs::read(store.root_certificate_path()).unwrap(), root_before);
    assert!(PkiStore::init(&temp_dir(), &Zeroizing::new(String::new())).is_err());

    fs::remove_dir_all(data_dir).unwrap();
}

#[test]
fn concurrent_initialization_publishes_one_complete_pki() {
    let data_dir = temp_dir();
    let barrier = Arc::new(Barrier::new(2));
    let handles = [(), ()].map(|()| {
        let data_dir = data_dir.clone();
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            barrier.wait();
            PkiStore::init(&data_dir, &Zeroizing::new("ca-password".to_string()))
        })
    });
    let [first, second] = handles;
    let results = [first.join().unwrap(), second.join().unwrap()];

    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert!(PkiStore::open(&data_dir).is_ok());

    fs::remove_dir_all(data_dir).unwrap();
}

#[test]
fn edge_issue_request_exposes_authorized_identity_and_csr() {
    let request = EdgeIssueRequest { edge_id: "edge-prod-01", csr_der: &[1, 2, 3], validity: time::Duration::days(30) };

    assert_eq!(request.edge_id, "edge-prod-01");
    assert_eq!(request.csr_der, [1, 2, 3]);
}

#[test]
fn ca_private_keys_are_encrypted_and_permission_restricted() {
    let data_dir = temp_dir();
    let password = Zeroizing::new("ca-password".to_string());
    PkiStore::init(&data_dir, &password).unwrap();

    for role in ["root", "server", "edge", "client"] {
        let key_path = data_dir.join(role).join("ca.key.encrypted.pem");
        let pem = fs::read_to_string(&key_path).unwrap();
        assert!(pem.starts_with("-----BEGIN ENCRYPTED PRIVATE KEY-----"));
        let (_, key_pem) = parse_x509_pem(pem.as_bytes()).unwrap();
        let encrypted = EncryptedPrivateKeyInfoRef::try_from(key_pem.contents.as_slice()).unwrap();
        assert!(encrypted.decrypt(password.as_bytes()).is_ok());
        assert!(encrypted.decrypt(b"wrong-password").is_err());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(key_path).unwrap().permissions().mode(), 0o100600);
        }
    }

    fs::remove_dir_all(data_dir).unwrap();
}

#[test]
fn issues_role_limited_certificates_and_modern_client_bundle() {
    let data_dir = temp_dir();
    let ca_password = Zeroizing::new("ca-password".to_string());
    let bundle_password = Zeroizing::new("bundle-password".to_string());
    let store = PkiStore::init(&data_dir, &ca_password).unwrap();
    let validity = time::Duration::days(30);

    let server = store
        .issue_server(
            ServerIssueRequest {
                name: "main",
                dns_sans: &["gateway.example.com"],
                ip_sans: &[IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10))],
                validity,
            },
            &ca_password,
        )
        .unwrap();

    let csr_key = KeyPair::generate().unwrap();
    let mut csr_params = CertificateParams::default();
    csr_params.distinguished_name.push(DnType::CommonName, "attacker-controlled");
    csr_params.subject_alt_names.push(SanType::URI("urn:dbx-gateway:edge:attacker".try_into().unwrap()));
    let csr = csr_params.serialize_request(&csr_key).unwrap();
    let mut tampered_csr = csr.der().to_vec();
    *tampered_csr.last_mut().unwrap() ^= 1;
    assert!(store
        .issue_edge(EdgeIssueRequest { edge_id: "edge-prod-01", csr_der: &tampered_csr, validity }, &ca_password,)
        .is_err());
    let edge = store
        .issue_edge(EdgeIssueRequest { edge_id: "edge-prod-01", csr_der: csr.der(), validity }, &ca_password)
        .unwrap();
    let client = store
        .issue_client(
            ClientIssueRequest { client_id: "laptop-01", validity, bundle_password: &bundle_password },
            &ca_password,
        )
        .unwrap();

    let (_, server_pem) = parse_x509_pem(server.issued.certificate_pem.as_bytes()).unwrap();
    let (_, server_cert) = parse_x509_certificate(&server_pem.contents).unwrap();
    let server_eku = server_cert.extended_key_usage().unwrap().unwrap();
    assert!(server_eku.value.server_auth);
    assert!(!server_eku.value.client_auth);
    let server_san = server_cert.subject_alternative_name().unwrap().unwrap();
    assert!(server_san
        .value
        .general_names
        .iter()
        .any(|name| matches!(name, GeneralName::DNSName("gateway.example.com"))));
    assert!(server_san
        .value
        .general_names
        .iter()
        .any(|name| matches!(name, GeneralName::IPAddress(bytes) if *bytes == [192, 0, 2, 10])));

    let (_, edge_pem) = parse_x509_pem(edge.certificate_pem.as_bytes()).unwrap();
    let (_, edge_cert) = parse_x509_certificate(&edge_pem.contents).unwrap();
    let edge_eku = edge_cert.extended_key_usage().unwrap().unwrap();
    assert!(!edge_eku.value.server_auth);
    assert!(edge_eku.value.client_auth);
    let edge_san = edge_cert.subject_alternative_name().unwrap().unwrap();
    let edge_uris = edge_san
        .value
        .general_names
        .iter()
        .filter_map(|name| match name {
            GeneralName::URI(uri) => Some(*uri),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(edge_uris, ["urn:dbx-gateway:edge:edge-prod-01"]);
    assert_eq!(edge_cert.public_key().subject_public_key.data.as_ref(), csr_key.public_key_raw());

    let (_, client_pem) = parse_x509_pem(client.key_pair.issued.certificate_pem.as_bytes()).unwrap();
    let (_, client_cert) = parse_x509_certificate(&client_pem.contents).unwrap();
    let client_eku = client_cert.extended_key_usage().unwrap().unwrap();
    assert!(!client_eku.value.server_auth);
    assert!(client_eku.value.client_auth);
    let client_san = client_cert.subject_alternative_name().unwrap().unwrap();
    assert!(client_san
        .value
        .general_names
        .iter()
        .any(|name| matches!(name, GeneralName::URI("urn:dbx-gateway:client:laptop-01"))));

    assert_ne!(server_cert.issuer(), edge_cert.issuer());
    assert_ne!(server_cert.issuer(), client_cert.issuer());
    assert_ne!(edge_cert.issuer(), client_cert.issuer());
    assert_ne!(server.issued.serial_hex, edge.serial_hex);
    assert_ne!(edge.serial_hex, client.key_pair.issued.serial_hex);

    let bundle = KeyStore::from_pkcs12(&client.pkcs12_der, &bundle_password).unwrap();
    let (_, key_chain) = bundle.private_key_chain().unwrap();
    PrivateKeyInfoRef::from_der(key_chain.key()).unwrap();
    assert_eq!(key_chain.chain().len(), 3);

    fs::remove_dir_all(data_dir).unwrap();
}

#[test]
fn rejects_ip_literals_in_server_dns_sans() {
    let data_dir = temp_dir();
    let ca_password = Zeroizing::new("ca-password".to_string());
    let store = PkiStore::init(&data_dir, &ca_password).unwrap();

    let result = store.issue_server(
        ServerIssueRequest {
            name: "main",
            dns_sans: &["192.0.2.53"],
            ip_sans: &[],
            validity: time::Duration::days(30),
        },
        &ca_password,
    );

    assert!(result.is_err());
    fs::remove_dir_all(data_dir).unwrap();
}

#[test]
fn rejects_leaf_validity_beyond_the_issuing_ca() {
    let data_dir = temp_dir();
    let ca_password = Zeroizing::new("ca-password".to_string());
    let store = PkiStore::init(&data_dir, &ca_password).unwrap();

    let result = store.issue_server(
        ServerIssueRequest {
            name: "main",
            dns_sans: &["gateway.example.com"],
            ip_sans: &[],
            validity: time::Duration::days(4000),
        },
        &ca_password,
    );

    assert!(result.err().unwrap().message.contains("validity"));
    fs::remove_dir_all(data_dir).unwrap();
}

#[cfg(unix)]
#[test]
fn opening_pki_rejects_symlinked_private_keys() {
    use std::os::unix::fs::symlink;

    let data_dir = temp_dir();
    let ca_password = Zeroizing::new("ca-password".to_string());
    PkiStore::init(&data_dir, &ca_password).unwrap();
    let edge_key = data_dir.join("edge/ca.key.encrypted.pem");
    fs::remove_file(&edge_key).unwrap();
    symlink(data_dir.join("server/ca.key.encrypted.pem"), &edge_key).unwrap();

    assert!(PkiStore::open(&data_dir).is_err());
    fs::remove_dir_all(data_dir).unwrap();
}

#[test]
fn output_files_are_not_overwritten() {
    let data_dir = temp_dir();
    let output = data_dir.join("private-key.pem");
    fs::write(&output, b"existing secret").unwrap();

    assert!(write_output_file(&output, b"replacement", 0o600).is_err());
    assert_eq!(fs::read(&output).unwrap(), b"existing secret");
    fs::remove_dir_all(data_dir).unwrap();
}

#[test]
fn revocation_persists_and_generates_a_signed_monotonic_crl() {
    let data_dir = temp_dir();
    let ca_password = Zeroizing::new("ca-password".to_string());
    let store = PkiStore::init(&data_dir, &ca_password).unwrap();
    let validity = time::Duration::days(30);
    let first = store
        .issue_server(
            ServerIssueRequest { name: "main-a", dns_sans: &["a.gateway.example.com"], ip_sans: &[], validity },
            &ca_password,
        )
        .unwrap();
    let second = store
        .issue_server(
            ServerIssueRequest { name: "main-b", dns_sans: &["b.gateway.example.com"], ip_sans: &[], validity },
            &ca_password,
        )
        .unwrap();
    let third = store
        .issue_server(
            ServerIssueRequest { name: "main-c", dns_sans: &["c.gateway.example.com"], ip_sans: &[], validity },
            &ca_password,
        )
        .unwrap();

    let wrong_password_error = store
        .revoke(
            CertificateRole::Server,
            &third.issued.serial_hex,
            RevocationReason::Unspecified,
            &Zeroizing::new("wrong-password".to_string()),
        )
        .unwrap_err();
    assert!(wrong_password_error.message.contains("password"));

    let first_crl = store
        .revoke(CertificateRole::Server, &first.issued.serial_hex, RevocationReason::KeyCompromise, &ca_password)
        .unwrap();
    let (_, first_pem) = parse_x509_pem(first_crl.pem.as_bytes()).unwrap();
    let (_, parsed_first_crl) = parse_x509_crl(&first_pem.contents).unwrap();
    let (_, issuer_pem) = parse_x509_pem(fs::read(store.server_ca_certificate_path()).unwrap().as_slice()).unwrap();
    let (_, issuer) = parse_x509_certificate(&issuer_pem.contents).unwrap();

    assert_eq!(parsed_first_crl.issuer(), issuer.subject());
    parsed_first_crl.verify_signature(&issuer.tbs_certificate.subject_pki).unwrap();
    assert_eq!(parsed_first_crl.crl_number().unwrap().to_u64_digits(), [first_crl.number]);
    assert!(parsed_first_crl
        .iter_revoked_certificates()
        .any(|entry| { hex::encode(entry.user_certificate.to_bytes_be()) == first.issued.serial_hex }));

    let second_crl = store
        .revoke(
            CertificateRole::Server,
            &second.issued.serial_hex,
            RevocationReason::CessationOfOperation,
            &ca_password,
        )
        .unwrap();
    assert!(second_crl.number > first_crl.number);
    assert!(store
        .revoke(CertificateRole::Server, &first.issued.serial_hex, RevocationReason::Unspecified, &ca_password,)
        .is_err());
    assert!(store.revoke(CertificateRole::Server, "0123", RevocationReason::Unspecified, &ca_password,).is_err());
    assert!(store.revoke(CertificateRole::Server, "abc", RevocationReason::Unspecified, &ca_password,).is_err());
    assert!(store
        .revoke(CertificateRole::Server, &"aa".repeat(21), RevocationReason::Unspecified, &ca_password,)
        .is_err());

    fs::remove_dir_all(data_dir).unwrap();
}

#[test]
fn revocation_recovers_when_record_was_committed_before_crl() {
    let data_dir = temp_dir();
    let ca_password = Zeroizing::new("ca-password".to_string());
    let store = PkiStore::init(&data_dir, &ca_password).unwrap();
    let issued = store
        .issue_server(
            ServerIssueRequest {
                name: "main",
                dns_sans: &["gateway.example.com"],
                ip_sans: &[],
                validity: time::Duration::days(30),
            },
            &ca_password,
        )
        .unwrap();
    let record_path = data_dir.join("server/issued").join(format!("{}.toml", issued.issued.serial_hex));
    let mut record = fs::read_to_string(&record_path).unwrap().replace("revoked = false", "revoked = true");
    record.push_str(&format!(
        "revoked_at = {}\nreason = \"key_compromise\"\n",
        time::OffsetDateTime::now_utc().unix_timestamp()
    ));
    fs::write(record_path, record).unwrap();

    let crl = store
        .revoke(CertificateRole::Server, &issued.issued.serial_hex, RevocationReason::KeyCompromise, &ca_password)
        .unwrap();
    let (_, pem) = parse_x509_pem(crl.pem.as_bytes()).unwrap();
    let (_, parsed) = parse_x509_crl(&pem.contents).unwrap();
    assert!(parsed
        .iter_revoked_certificates()
        .any(|entry| hex::encode(entry.user_certificate.to_bytes_be()) == issued.issued.serial_hex));

    fs::remove_dir_all(data_dir).unwrap();
}

#[test]
fn concurrent_revocations_preserve_all_entries_and_unique_numbers() {
    let data_dir = temp_dir();
    let ca_password = Zeroizing::new("ca-password".to_string());
    let store = PkiStore::init(&data_dir, &ca_password).unwrap();
    let validity = time::Duration::days(30);
    let first = store
        .issue_server(
            ServerIssueRequest { name: "main-a", dns_sans: &["a.gateway.example.com"], ip_sans: &[], validity },
            &ca_password,
        )
        .unwrap();
    let second = store
        .issue_server(
            ServerIssueRequest { name: "main-b", dns_sans: &["b.gateway.example.com"], ip_sans: &[], validity },
            &ca_password,
        )
        .unwrap();
    let barrier = Arc::new(Barrier::new(2));

    let handles = [first.issued.serial_hex.clone(), second.issued.serial_hex.clone()].map(|serial| {
        let data_dir = data_dir.clone();
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            let store = PkiStore::open(&data_dir).unwrap();
            let password = Zeroizing::new("ca-password".to_string());
            barrier.wait();
            store.revoke(CertificateRole::Server, &serial, RevocationReason::Unspecified, &password)
        })
    });
    let [first_handle, second_handle] = handles;
    let first_crl = first_handle.join().unwrap().unwrap();
    let second_crl = second_handle.join().unwrap().unwrap();

    assert_ne!(first_crl.number, second_crl.number);
    let current_crl = fs::read(data_dir.join("server/crl.pem")).unwrap();
    let (_, pem) = parse_x509_pem(&current_crl).unwrap();
    let (_, parsed) = parse_x509_crl(&pem.contents).unwrap();
    let revoked = parsed
        .iter_revoked_certificates()
        .map(|entry| hex::encode(entry.user_certificate.to_bytes_be()))
        .collect::<Vec<_>>();
    assert!(revoked.contains(&first.issued.serial_hex));
    assert!(revoked.contains(&second.issued.serial_hex));

    fs::remove_dir_all(data_dir).unwrap();
}
