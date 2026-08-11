#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GatewayErrorCode {
    ConfigInvalid,
    TlsRejected,
    IdentityRejected,
    ProtocolMismatch,
    RouteDenied,
    EdgeOffline,
    TargetUnavailable,
    CapacityExceeded,
    Internal,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct GatewayError {
    pub code: GatewayErrorCode,
    pub message: String,
}
