use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{GatewayError, GatewayErrorCode};

pub const PROTOCOL_MAJOR: u16 = 1;
pub const PROTOCOL_MINOR: u16 = 0;
pub const MAX_CONTROL_FRAME_SIZE: usize = 64 * 1024;
pub const MAX_DATA_FRAME_SIZE: usize = 1024 * 1024;
pub const MAX_SESSION_TICKET_TTL: Duration = Duration::from_secs(5 * 60);
pub const DEFAULT_MAX_SESSION_TICKETS: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
}

impl ProtocolVersion {
    pub const fn new(major: u16, minor: u16) -> Self {
        Self { major, minor, capabilities: Vec::new() }
    }

    pub const fn current() -> Self {
        Self::new(PROTOCOL_MAJOR, PROTOCOL_MINOR)
    }

    pub fn ensure_compatible(&self) -> Result<(), GatewayError> {
        if self.major != PROTOCOL_MAJOR {
            return Err(protocol_error(GatewayErrorCode::ProtocolMismatch, "incompatible protocol version"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    OpenRoute { version: ProtocolVersion, request_id: Uuid, edge_id: String, target_id: String },
    ListRoutes { version: ProtocolVersion, request_id: Uuid },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientEvent {
    Stage { stage: Stage },
    Routes { edges: Vec<GatewayEdgeRoutes> },
    Error { code: GatewayErrorCode },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayRoute {
    pub target_id: String,
    pub display_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayEdgeRoutes {
    pub edge_id: String,
    pub online: bool,
    pub routes: Vec<GatewayRoute>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MainToEdge {
    OpenDataChannel { session_id: SessionId, target_id: String, expires_at_unix_ms: i64 },
    HeartbeatAck { unix_ms: i64 },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EdgeToMain {
    Heartbeat { version: ProtocolVersion },
    DataChannelFailed { version: ProtocolVersion, session_id: SessionId },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    MainAuthenticated,
    RouteAuthorized,
    EdgeChannelReady,
    TargetConnected,
    StreamReady,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeRegistration {
    pub version: ProtocolVersion,
    pub edge_id: String,
    pub targets: Vec<RegisteredTarget>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeDataOpen {
    pub version: ProtocolVersion,
    pub session_id: SessionId,
    pub target_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisteredTarget {
    pub target_id: String,
    pub display_name: String,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(Uuid);

impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionTicket {
    pub session_id: SessionId,
    pub expires_at_unix_ms: i64,
}

struct TicketBinding {
    edge_id: String,
    target_id: String,
    client_id: String,
    expires_at: Instant,
}

pub struct SessionTickets {
    ttl: Duration,
    max_entries: usize,
    entries: HashMap<SessionId, TicketBinding>,
    expirations: BTreeSet<(Instant, SessionId)>,
}

impl SessionTickets {
    pub fn new(ttl: Duration) -> Self {
        Self::with_capacity(ttl, DEFAULT_MAX_SESSION_TICKETS)
    }

    pub fn with_capacity(ttl: Duration, max_entries: usize) -> Self {
        Self {
            ttl: ttl.min(MAX_SESSION_TICKET_TTL),
            max_entries,
            entries: HashMap::new(),
            expirations: BTreeSet::new(),
        }
    }

    pub fn issue(&mut self, edge_id: &str, target_id: &str, client_id: &str) -> Result<SessionTicket, GatewayError> {
        let now = Instant::now();
        self.purge_expired(now);
        if self.entries.len() >= self.max_entries {
            return Err(protocol_error(GatewayErrorCode::CapacityExceeded, "session ticket capacity exceeded"));
        }
        let session_id = loop {
            let candidate = SessionId::new();
            if !self.entries.contains_key(&candidate) {
                break candidate;
            }
        };
        let expires_at = now.checked_add(self.ttl).unwrap_or(now);
        let expires_at_unix_ms = unix_time_after(self.ttl);
        self.entries.insert(
            session_id,
            TicketBinding {
                edge_id: edge_id.to_string(),
                target_id: target_id.to_string(),
                client_id: client_id.to_string(),
                expires_at,
            },
        );
        self.expirations.insert((expires_at, session_id));
        Ok(SessionTicket { session_id, expires_at_unix_ms })
    }

    pub fn consume(
        &mut self,
        session_id: &SessionId,
        edge_id: &str,
        target_id: &str,
        client_id: &str,
    ) -> Result<(), GatewayError> {
        let now = Instant::now();
        let Some(binding) = self.entries.get(session_id) else {
            return Err(ticket_rejected());
        };
        let expires_at = binding.expires_at;
        if binding.expires_at <= now {
            self.entries.remove(session_id);
            self.expirations.remove(&(expires_at, *session_id));
            return Err(ticket_rejected());
        }
        if binding.edge_id != edge_id || binding.target_id != target_id || binding.client_id != client_id {
            return Err(ticket_rejected());
        }
        self.entries.remove(session_id);
        self.expirations.remove(&(expires_at, *session_id));
        Ok(())
    }

    pub fn discard(&mut self, session_id: &SessionId) {
        if let Some(binding) = self.entries.remove(session_id) {
            self.expirations.remove(&(binding.expires_at, *session_id));
        }
    }

    fn purge_expired(&mut self, now: Instant) {
        while let Some(&(expires_at, session_id)) = self.expirations.first() {
            if expires_at > now {
                break;
            }
            self.expirations.remove(&(expires_at, session_id));
            if self.entries.get(&session_id).is_some_and(|binding| binding.expires_at == expires_at) {
                self.entries.remove(&session_id);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnauthenticatedRejection {
    Rejected,
}

pub fn encode_control_frame<T: Serialize>(message: &T) -> Result<Vec<u8>, GatewayError> {
    let frame = serde_json::to_vec(message)
        .map_err(|_| protocol_error(GatewayErrorCode::ProtocolMismatch, "invalid control message"))?;
    if frame.len() > MAX_CONTROL_FRAME_SIZE {
        return Err(protocol_error(GatewayErrorCode::CapacityExceeded, "control frame is too large"));
    }
    Ok(frame)
}

pub fn decode_control_frame<T: DeserializeOwned>(frame: &[u8]) -> Result<T, GatewayError> {
    if frame.len() > MAX_CONTROL_FRAME_SIZE {
        return Err(protocol_error(GatewayErrorCode::CapacityExceeded, "control frame is too large"));
    }
    serde_json::from_slice(frame)
        .map_err(|_| protocol_error(GatewayErrorCode::ProtocolMismatch, "invalid control message"))
}

pub fn decode_client_message(frame: &[u8]) -> Result<ClientMessage, GatewayError> {
    let message: ClientMessage = decode_control_frame(frame)?;
    match &message {
        ClientMessage::OpenRoute { version, .. } | ClientMessage::ListRoutes { version, .. } => {
            version.ensure_compatible()?
        }
    }
    Ok(message)
}

pub fn decode_edge_registration(frame: &[u8]) -> Result<EdgeRegistration, GatewayError> {
    let registration: EdgeRegistration = decode_control_frame(frame)?;
    registration.version.ensure_compatible()?;
    Ok(registration)
}

pub fn decode_edge_message(frame: &[u8]) -> Result<EdgeToMain, GatewayError> {
    let message: EdgeToMain = decode_control_frame(frame)?;
    match &message {
        EdgeToMain::Heartbeat { version } | EdgeToMain::DataChannelFailed { version, .. } => {
            version.ensure_compatible()?
        }
    }
    Ok(message)
}

pub fn decode_edge_data_open(frame: &[u8]) -> Result<EdgeDataOpen, GatewayError> {
    let message: EdgeDataOpen = decode_control_frame(frame)?;
    message.version.ensure_compatible()?;
    Ok(message)
}

pub fn decode_unauthenticated_control_frame<T: DeserializeOwned>(frame: &[u8]) -> Result<T, UnauthenticatedRejection> {
    decode_control_frame(frame).map_err(|_| UnauthenticatedRejection::Rejected)
}

fn unix_time_after(duration: Duration) -> i64 {
    SystemTime::now()
        .checked_add(duration)
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(i64::MAX)
}

fn ticket_rejected() -> GatewayError {
    protocol_error(GatewayErrorCode::IdentityRejected, "session ticket rejected")
}

fn protocol_error(code: GatewayErrorCode, message: &str) -> GatewayError {
    GatewayError { code, message: message.to_string() }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use uuid::Uuid;

    use super::*;
    use crate::GatewayErrorCode;

    #[test]
    fn current_protocol_version_is_fixed() {
        assert_eq!(PROTOCOL_MAJOR, 1);
        assert_eq!(PROTOCOL_MINOR, 0);
        assert_eq!(ProtocolVersion::current(), ProtocolVersion::new(1, 0));
    }

    #[test]
    fn incompatible_major_version_is_rejected() {
        let error = ProtocolVersion::new(PROTOCOL_MAJOR + 1, 0).ensure_compatible().unwrap_err();

        assert_eq!(error.code, GatewayErrorCode::ProtocolMismatch);
    }

    #[test]
    fn unknown_minor_version_and_capabilities_are_ignored() {
        let frame = serde_json::to_vec(&json!({
            "type": "open_route",
            "version": {
                "major": PROTOCOL_MAJOR,
                "minor": PROTOCOL_MINOR + 7,
                "capabilities": ["future-capability"]
            },
            "request_id": Uuid::new_v4(),
            "edge_id": "edge-1",
            "target_id": "postgres",
            "future_field": true
        }))
        .unwrap();

        let ClientMessage::OpenRoute { version, .. } = decode_client_message(&frame).unwrap() else {
            panic!("expected open route");
        };
        assert!(version.ensure_compatible().is_ok());
    }

    #[test]
    fn route_discovery_round_trip_contains_only_logical_routes() {
        let event = ClientEvent::Routes {
            edges: vec![GatewayEdgeRoutes {
                edge_id: "edge-prod-01".to_string(),
                online: true,
                routes: vec![GatewayRoute {
                    target_id: "postgres-primary".to_string(),
                    display_name: "Primary PostgreSQL".to_string(),
                }],
            }],
        };

        let frame = encode_control_frame(&event).unwrap();
        let decoded: ClientEvent = decode_control_frame(&frame).unwrap();
        assert_eq!(decoded, event);
        assert!(!String::from_utf8(frame).unwrap().contains("5432"));
    }

    #[test]
    fn typed_decoders_reject_incompatible_major_versions() {
        let client = serde_json::to_vec(&json!({
            "type": "open_route",
            "version": { "major": PROTOCOL_MAJOR + 1, "minor": 0 },
            "request_id": Uuid::new_v4(),
            "edge_id": "edge-1",
            "target_id": "postgres"
        }))
        .unwrap();
        let registration = serde_json::to_vec(&json!({
            "version": { "major": PROTOCOL_MAJOR + 1, "minor": 0 },
            "edge_id": "edge-1",
            "targets": []
        }))
        .unwrap();

        assert_eq!(decode_client_message(&client).unwrap_err().code, GatewayErrorCode::ProtocolMismatch);
        assert_eq!(decode_edge_registration(&registration).unwrap_err().code, GatewayErrorCode::ProtocolMismatch);
    }

    #[test]
    fn edge_registration_does_not_expose_target_address() {
        let registration = EdgeRegistration {
            version: ProtocolVersion::current(),
            edge_id: "edge-1".to_string(),
            targets: vec![RegisteredTarget {
                target_id: "postgres".to_string(),
                display_name: "PostgreSQL".to_string(),
            }],
        };

        let value = serde_json::to_value(registration).unwrap();
        assert_eq!(value["targets"][0], json!({ "target_id": "postgres", "display_name": "PostgreSQL" }));
    }

    #[test]
    fn fixed_messages_and_stages_have_stable_json_names() {
        let session_id = SessionId::new();
        let message =
            MainToEdge::OpenDataChannel { session_id, target_id: "postgres".to_string(), expires_at_unix_ms: 42 };
        assert_eq!(serde_json::to_value(message).unwrap()["type"], "open_data_channel");
        assert_eq!(serde_json::to_value(MainToEdge::HeartbeatAck { unix_ms: 42 }).unwrap()["type"], "heartbeat_ack");

        let stages = [
            (Stage::MainAuthenticated, "main_authenticated"),
            (Stage::RouteAuthorized, "route_authorized"),
            (Stage::EdgeChannelReady, "edge_channel_ready"),
            (Stage::TargetConnected, "target_connected"),
            (Stage::StreamReady, "stream_ready"),
        ];
        for (stage, expected) in stages {
            assert_eq!(serde_json::to_value(stage).unwrap(), expected);
        }
    }

    #[test]
    fn session_tickets_are_random_and_identity_bound() {
        let mut tickets = SessionTickets::new(Duration::from_secs(15));
        let first = tickets.issue("edge-1", "postgres", "client-1").unwrap();
        let second = tickets.issue("edge-1", "postgres", "client-1").unwrap();
        assert_ne!(first.session_id, second.session_id);

        let error = tickets.consume(&first.session_id, "edge-2", "postgres", "client-1").unwrap_err();
        assert_eq!(error.code, GatewayErrorCode::IdentityRejected);
        let error = tickets.consume(&first.session_id, "edge-1", "mysql", "client-1").unwrap_err();
        assert_eq!(error.code, GatewayErrorCode::IdentityRejected);
        let error = tickets.consume(&first.session_id, "edge-1", "postgres", "client-2").unwrap_err();
        assert_eq!(error.code, GatewayErrorCode::IdentityRejected);

        assert!(tickets.consume(&first.session_id, "edge-1", "postgres", "client-1").is_ok());
    }

    #[test]
    fn session_ticket_expires() {
        let mut tickets = SessionTickets::new(Duration::ZERO);
        let ticket = tickets.issue("edge-1", "postgres", "client-1").unwrap();

        let error = tickets.consume(&ticket.session_id, "edge-1", "postgres", "client-1").unwrap_err();
        assert_eq!(error.code, GatewayErrorCode::IdentityRejected);
    }

    #[test]
    fn session_ticket_is_single_use() {
        let mut tickets = SessionTickets::new(Duration::from_secs(15));
        let ticket = tickets.issue("edge-1", "postgres", "client-1").unwrap();
        assert!(tickets.consume(&ticket.session_id, "edge-1", "postgres", "client-1").is_ok());

        let error = tickets.consume(&ticket.session_id, "edge-1", "postgres", "client-1").unwrap_err();
        assert_eq!(error.code, GatewayErrorCode::IdentityRejected);
    }

    #[test]
    fn session_ticket_capacity_is_bounded_and_released_on_consume() {
        let mut tickets = SessionTickets::with_capacity(Duration::from_secs(15), 1);
        let first = tickets.issue("edge-1", "postgres", "client-1").unwrap();

        let error = tickets.issue("edge-1", "mysql", "client-1").unwrap_err();
        assert_eq!(error.code, GatewayErrorCode::CapacityExceeded);
        tickets.consume(&first.session_id, "edge-1", "postgres", "client-1").unwrap();
        assert!(tickets.issue("edge-1", "mysql", "client-1").is_ok());
    }

    #[test]
    fn expired_session_tickets_release_capacity() {
        let mut tickets = SessionTickets::with_capacity(Duration::ZERO, 1);
        tickets.issue("edge-1", "postgres", "client-1").unwrap();

        assert!(tickets.issue("edge-1", "mysql", "client-1").is_ok());
    }

    #[test]
    fn session_ticket_ttl_is_capped_to_a_short_lived_limit() {
        let tickets = SessionTickets::new(Duration::MAX);

        assert_eq!(tickets.ttl, MAX_SESSION_TICKET_TTL);
    }

    #[test]
    fn oversized_control_frame_is_rejected_before_json_parsing() {
        let frame = vec![b'x'; MAX_CONTROL_FRAME_SIZE + 1];

        let error = decode_control_frame::<ClientMessage>(&frame).unwrap_err();
        assert_eq!(error.code, GatewayErrorCode::CapacityExceeded);
    }

    #[test]
    fn unauthenticated_decode_only_exposes_an_opaque_rejection() {
        let malformed = b"internal database target: 10.0.0.8:5432";
        let oversized = vec![b'x'; MAX_CONTROL_FRAME_SIZE + 1];

        assert_eq!(
            decode_unauthenticated_control_frame::<ClientMessage>(malformed).unwrap_err(),
            UnauthenticatedRejection::Rejected
        );
        assert_eq!(
            decode_unauthenticated_control_frame::<ClientMessage>(&oversized).unwrap_err(),
            UnauthenticatedRejection::Rejected
        );
    }
}
