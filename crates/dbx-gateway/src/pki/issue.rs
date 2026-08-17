use std::net::IpAddr;

use p12_keystore::{
    Certificate as P12Certificate, EncryptionAlgorithm, KeyStore, KeyStoreEntry, MacAlgorithm, PrivateKeyChain,
};
use rcgen::{
    CertificateParams, CertificateSigningRequestParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose, SanType, SerialNumber,
};
use time::OffsetDateTime;
use x509_parser::pem::parse_x509_pem;
use x509_parser::prelude::parse_x509_certificate;
use zeroize::Zeroizing;

use super::store::{load_issuer, pki_error, read_certificate, read_optional_certificate, record_issued_certificate};
use super::{CertificateRole, PkiStore};
use crate::GatewayError;

pub struct EdgeIssueRequest<'a> {
    pub edge_id: &'a str,
    pub csr_der: &'a [u8],
    pub validity: time::Duration,
}

pub struct ServerIssueRequest<'a> {
    pub name: &'a str,
    pub dns_sans: &'a [&'a str],
    pub ip_sans: &'a [IpAddr],
    pub validity: time::Duration,
}

pub struct ClientIssueRequest<'a> {
    pub client_id: &'a str,
    pub validity: time::Duration,
    pub bundle_password: &'a Zeroizing<String>,
}

pub struct IssuedCertificate {
    pub serial_hex: String,
    pub certificate_pem: String,
    pub chain_pem: String,
}

pub struct IssuedKeyPair {
    pub issued: IssuedCertificate,
    pub private_key_pem: Zeroizing<String>,
}

pub struct IssuedClientBundle {
    pub key_pair: IssuedKeyPair,
    pub pkcs12_der: Zeroizing<Vec<u8>>,
}

impl PkiStore {
    pub fn issue_server(
        &self,
        request: ServerIssueRequest<'_>,
        ca_password: &Zeroizing<String>,
    ) -> Result<IssuedKeyPair, GatewayError> {
        if request.dns_sans.is_empty() && request.ip_sans.is_empty() {
            return Err(pki_error("server certificate requires at least one SAN"));
        }
        if request.dns_sans.iter().any(|name| name.parse::<IpAddr>().is_ok()) {
            return Err(pki_error("server IP addresses must use an IP SAN"));
        }
        let key = KeyPair::generate().map_err(|_| pki_error("could not generate server private key"))?;
        let mut params = leaf_params(request.name, request.validity)?;
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.subject_alt_names = request
            .dns_sans
            .iter()
            .map(|name| Ok(SanType::DnsName((*name).try_into().map_err(|_| pki_error("invalid server DNS SAN"))?)))
            .collect::<Result<Vec<_>, GatewayError>>()?;
        params.subject_alt_names.extend(request.ip_sans.iter().copied().map(SanType::IpAddress));
        self.issue_with_key(CertificateRole::Server, request.name, params, &key, ca_password)
    }

    pub fn issue_edge(
        &self,
        request: EdgeIssueRequest<'_>,
        ca_password: &Zeroizing<String>,
    ) -> Result<IssuedCertificate, GatewayError> {
        validate_id(request.edge_id)?;
        let csr_der = request.csr_der.into();
        let csr =
            CertificateSigningRequestParams::from_der(&csr_der).map_err(|_| pki_error("edge CSR was rejected"))?;
        let mut params = leaf_params(request.edge_id, request.validity)?;
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        params.subject_alt_names = vec![SanType::URI(
            format!("urn:dbx-gateway:edge:{}", request.edge_id)
                .try_into()
                .map_err(|_| pki_error("invalid edge identity"))?,
        )];
        validate_issuer_validity(&self.data_dir, CertificateRole::Edge, params.not_after)?;
        let issuer = load_issuer(&self.data_dir, CertificateRole::Edge.as_str(), ca_password)?;
        let certificate =
            params.signed_by(&csr.public_key, &issuer).map_err(|_| pki_error("could not issue edge certificate"))?;
        issued_certificate(
            &self.data_dir,
            CertificateRole::Edge,
            request.edge_id,
            certificate.pem(),
            params.serial_number,
        )
    }

    pub fn issue_client(
        &self,
        request: ClientIssueRequest<'_>,
        ca_password: &Zeroizing<String>,
    ) -> Result<IssuedClientBundle, GatewayError> {
        validate_id(request.client_id)?;
        if request.bundle_password.is_empty() {
            return Err(pki_error("bundle password must not be empty"));
        }
        let key = KeyPair::generate().map_err(|_| pki_error("could not generate client private key"))?;
        let mut params = leaf_params(request.client_id, request.validity)?;
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        params.subject_alt_names = vec![SanType::URI(
            format!("urn:dbx-gateway:client:{}", request.client_id)
                .try_into()
                .map_err(|_| pki_error("invalid client identity"))?,
        )];
        let key_pair = self.issue_with_key(CertificateRole::Client, request.client_id, params, &key, ca_password)?;
        let chain = certificate_chain_der(&key_pair.issued.certificate_pem, &key_pair.issued.chain_pem)?;
        let mut store = KeyStore::new();
        store.add_entry(
            request.client_id,
            KeyStoreEntry::PrivateKeyChain(PrivateKeyChain::new(
                key.serialize_der(),
                key_pair.issued.serial_hex.as_bytes(),
                chain,
            )),
        );
        let pkcs12_der = store
            .writer(request.bundle_password)
            .encryption_algorithm(EncryptionAlgorithm::PbeWithHmacSha256AndAes256)
            .mac_algorithm(MacAlgorithm::HmacSha256)
            .write()
            .map_err(|_| pki_error("could not create client PKCS#12 bundle"))?;
        Ok(IssuedClientBundle { key_pair, pkcs12_der: Zeroizing::new(pkcs12_der) })
    }

    fn issue_with_key(
        &self,
        role: CertificateRole,
        identity: &str,
        params: CertificateParams,
        key: &KeyPair,
        ca_password: &Zeroizing<String>,
    ) -> Result<IssuedKeyPair, GatewayError> {
        let serial = params.serial_number.clone();
        validate_issuer_validity(&self.data_dir, role, params.not_after)?;
        let issuer = load_issuer(&self.data_dir, role.as_str(), ca_password)?;
        let certificate = params.signed_by(key, &issuer).map_err(|_| pki_error("could not issue certificate"))?;
        let issued = issued_certificate(&self.data_dir, role, identity, certificate.pem(), serial)?;
        Ok(IssuedKeyPair { issued, private_key_pem: Zeroizing::new(key.serialize_pem()) })
    }
}

fn validate_issuer_validity(
    data_dir: &std::path::Path,
    role: CertificateRole,
    leaf_not_after: OffsetDateTime,
) -> Result<(), GatewayError> {
    let certificate_pem = read_certificate(data_dir, role.as_str())?;
    let (_, pem) =
        parse_x509_pem(certificate_pem.as_bytes()).map_err(|_| pki_error("could not parse CA certificate validity"))?;
    let (_, certificate) =
        parse_x509_certificate(&pem.contents).map_err(|_| pki_error("could not parse CA certificate validity"))?;
    if leaf_not_after > certificate.validity().not_after.to_datetime() {
        return Err(pki_error("certificate validity exceeds the issuing CA"));
    }
    Ok(())
}

fn leaf_params(name: &str, validity: time::Duration) -> Result<CertificateParams, GatewayError> {
    if name.trim().is_empty() || validity <= time::Duration::ZERO {
        return Err(pki_error("invalid certificate identity or validity"));
    }
    let now = OffsetDateTime::now_utc();
    let mut params = CertificateParams::default();
    params.distinguished_name.push(DnType::CommonName, name);
    params.is_ca = IsCa::ExplicitNoCa;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.not_before = now - time::Duration::minutes(5);
    params.not_after = now + validity;
    params.serial_number = Some(random_serial());
    Ok(params)
}

fn random_serial() -> SerialNumber {
    use p12_keystore::rand::Rng;

    let mut bytes = [0_u8; 16];
    p12_keystore::rand::rng().fill_bytes(&mut bytes);
    bytes[0] &= 0x7f;
    if bytes[0] == 0 {
        bytes[0] = 1;
    }
    SerialNumber::from(bytes.to_vec())
}

fn validate_id(id: &str) -> Result<(), GatewayError> {
    if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')) {
        return Err(pki_error("invalid certificate identity"));
    }
    Ok(())
}

fn issued_certificate(
    data_dir: &std::path::Path,
    role: CertificateRole,
    identity: &str,
    certificate_pem: String,
    serial: Option<SerialNumber>,
) -> Result<IssuedCertificate, GatewayError> {
    let serial_hex =
        hex::encode(serial.as_ref().ok_or_else(|| pki_error("certificate serial was not generated"))?.as_ref());
    record_issued_certificate(data_dir, role, &serial_hex, &certificate_pem, identity)?;
    let intermediate = read_certificate(data_dir, role.as_str())?;
    let root = read_optional_certificate(data_dir, "root")?.unwrap_or_default();
    Ok(IssuedCertificate { serial_hex, certificate_pem, chain_pem: format!("{intermediate}{root}") })
}

fn certificate_chain_der(leaf_pem: &str, chain_pem: &str) -> Result<Vec<P12Certificate>, GatewayError> {
    let mut certificates = Vec::new();
    let mut input = leaf_pem.as_bytes();
    while !input.is_empty() {
        let (rest, pem) = parse_x509_pem(input).map_err(|_| pki_error("could not parse certificate chain"))?;
        certificates.push(
            P12Certificate::from_der(&pem.contents)
                .map_err(|_| pki_error("could not create PKCS#12 certificate chain"))?,
        );
        input = rest;
    }
    input = chain_pem.as_bytes();
    while !input.is_empty() {
        let (rest, pem) = parse_x509_pem(input).map_err(|_| pki_error("could not parse certificate chain"))?;
        certificates.push(
            P12Certificate::from_der(&pem.contents)
                .map_err(|_| pki_error("could not create PKCS#12 certificate chain"))?,
        );
        input = rest;
    }
    Ok(certificates)
}
