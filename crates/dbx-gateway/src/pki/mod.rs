mod issue;
mod service;
mod store;

use std::fmt;
use std::str::FromStr;

pub use issue::{
    ClientIssueRequest, EdgeIssueRequest, IssuedCertificate, IssuedClientBundle, IssuedKeyPair, ServerIssueRequest,
};
pub use service::{
    enroll_over_remote, enroll_over_unix, renew_over_remote, renew_over_unix, serve_remote, serve_unix,
    EnrollCsrRequest, EnrollCsrResponse, PkiEnrollmentService, RemotePkiConfig, RemotePkiServer, RenewCsrRequest,
    UnixPkiServer,
};
pub use store::{write_output_file, GeneratedCrl, PkiStore};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CertificateRole {
    Server,
    Edge,
    Client,
}

impl CertificateRole {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Server => "server",
            Self::Edge => "edge",
            Self::Client => "client",
        }
    }
}

impl fmt::Display for CertificateRole {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for CertificateRole {
    type Err = crate::GatewayError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "server" => Ok(Self::Server),
            "edge" => Ok(Self::Edge),
            "client" => Ok(Self::Client),
            _ => Err(store::pki_error("invalid certificate role")),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RevocationReason {
    Unspecified,
    KeyCompromise,
    CaCompromise,
    AffiliationChanged,
    Superseded,
    CessationOfOperation,
    CertificateHold,
    PrivilegeWithdrawn,
    AaCompromise,
}

impl RevocationReason {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Unspecified => "unspecified",
            Self::KeyCompromise => "key_compromise",
            Self::CaCompromise => "ca_compromise",
            Self::AffiliationChanged => "affiliation_changed",
            Self::Superseded => "superseded",
            Self::CessationOfOperation => "cessation_of_operation",
            Self::CertificateHold => "certificate_hold",
            Self::PrivilegeWithdrawn => "privilege_withdrawn",
            Self::AaCompromise => "aa_compromise",
        }
    }
}

impl FromStr for RevocationReason {
    type Err = crate::GatewayError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "unspecified" => Ok(Self::Unspecified),
            "key_compromise" => Ok(Self::KeyCompromise),
            "ca_compromise" => Ok(Self::CaCompromise),
            "affiliation_changed" => Ok(Self::AffiliationChanged),
            "superseded" => Ok(Self::Superseded),
            "cessation_of_operation" => Ok(Self::CessationOfOperation),
            "certificate_hold" => Ok(Self::CertificateHold),
            "privilege_withdrawn" => Ok(Self::PrivilegeWithdrawn),
            "aa_compromise" => Ok(Self::AaCompromise),
            _ => Err(store::pki_error("invalid certificate revocation reason")),
        }
    }
}
