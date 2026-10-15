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

pub(crate) fn diagnostic_line(
    level: &str,
    component: &str,
    event: &str,
    fields: serde_json::Value,
) -> String {
    serde_json::json!({
        "level": level,
        "component": component,
        "event": event,
        "fields": fields,
    })
    .to_string()
}

pub(crate) fn log_diagnostic(
    level: &str,
    component: &str,
    event: &str,
    fields: serde_json::Value,
) {
    eprintln!("{}", diagnostic_line(level, component, event, fields));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_output_is_single_line_json() {
        let line = diagnostic_line(
            "warn",
            "edge",
            "control_connect_failed",
            serde_json::json!({ "detail": "quoted \"value\"\nnext", "retry_secs": 5 }),
        );
        assert_eq!(line.lines().count(), 1);
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["level"], "warn");
        assert_eq!(value["component"], "edge");
        assert_eq!(value["event"], "control_connect_failed");
        assert_eq!(value["fields"]["detail"], "quoted \"value\"\nnext");
        assert_eq!(value["fields"]["retry_secs"], 5);
    }
}
