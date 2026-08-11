use std::path::PathBuf;
use std::time::Duration;

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::state::{run_db, unix_now};
use crate::{GatewayError, GatewayErrorCode};

#[derive(Clone)]
pub struct EnrollmentStore {
    path: PathBuf,
}

pub struct EnrollmentToken {
    pub id: Uuid,
    pub edge_id: String,
    pub secret: Zeroizing<String>,
    pub created_at: time::OffsetDateTime,
    pub expires_at: time::OffsetDateTime,
    pub replace: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsumedEnrollment {
    pub token_id: Uuid,
    pub edge_id: String,
    pub replace: bool,
}

impl EnrollmentStore {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub async fn create(&self, edge_id: &str, ttl: Duration, replace: bool) -> Result<EnrollmentToken, GatewayError> {
        if edge_id.trim().is_empty() || ttl.is_zero() {
            return Err(enrollment_error(GatewayErrorCode::ConfigInvalid, "edge ID and token TTL are required"));
        }
        let ttl_secs = i64::try_from(ttl.as_secs())
            .map_err(|_| enrollment_error(GatewayErrorCode::ConfigInvalid, "token TTL is too large"))?;
        let id = Uuid::new_v4();
        let secret = Zeroizing::new(format!("{id}.{}", Uuid::new_v4()));
        let salt = SaltString::encode_b64(Uuid::new_v4().as_bytes())
            .map_err(|_| enrollment_error(GatewayErrorCode::Internal, "could not create enrollment token"))?;
        let token_hash = Argon2::default()
            .hash_password(secret.as_bytes(), &salt)
            .map_err(|_| enrollment_error(GatewayErrorCode::Internal, "could not create enrollment token"))?
            .to_string();
        let edge_id = edge_id.to_string();
        let created_at = unix_now();
        let expires_at = created_at
            .checked_add(ttl_secs)
            .ok_or_else(|| enrollment_error(GatewayErrorCode::ConfigInvalid, "token TTL is too large"))?;
        let stored_edge_id = edge_id.clone();
        let created = run_db(self.path.clone(), move |connection| {
            let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let active = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM issued_certificates WHERE edge_id = ?1 AND revoked_at IS NULL)",
                [&stored_edge_id],
                |row| row.get::<_, bool>(0),
            )?;
            if active && !replace {
                return Ok(false);
            }
            if active {
                transaction.execute(
                    "INSERT OR IGNORE INTO revocations (serial_hex, edge_id, revoked_at, reason)
                     SELECT serial_hex, edge_id, ?2, 'superseded' FROM issued_certificates
                     WHERE edge_id = ?1 AND revoked_at IS NULL",
                    params![stored_edge_id, created_at],
                )?;
                transaction.execute(
                    "UPDATE issued_certificates SET revoked_at = ?2 WHERE edge_id = ?1 AND revoked_at IS NULL",
                    params![stored_edge_id, created_at],
                )?;
            }
            transaction.execute(
                "INSERT INTO enrollments
                 (token_id, token_hash, edge_id, created_at, expires_at, consumed_at, revoked_at, replace_existing)
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL, ?6)",
                params![id.to_string(), token_hash, stored_edge_id, created_at, expires_at, replace],
            )?;
            transaction.commit()?;
            Ok(true)
        })
        .await?;
        if !created {
            return Err(enrollment_error(GatewayErrorCode::RouteDenied, "active edge certificate requires replace"));
        }
        Ok(EnrollmentToken {
            id,
            edge_id,
            secret,
            created_at: time::OffsetDateTime::from_unix_timestamp(created_at).unwrap(),
            expires_at: time::OffsetDateTime::from_unix_timestamp(expires_at).unwrap(),
            replace,
        })
    }

    pub async fn consume(&self, claimed_edge_id: &str, secret: &str) -> Result<ConsumedEnrollment, GatewayError> {
        let (token_id, _) = secret
            .split_once('.')
            .ok_or_else(|| enrollment_error(GatewayErrorCode::IdentityRejected, "enrollment token rejected"))?;
        let token_id = Uuid::parse_str(token_id)
            .map_err(|_| enrollment_error(GatewayErrorCode::IdentityRejected, "enrollment token rejected"))?;
        let claimed_edge_id = claimed_edge_id.to_string();
        let secret = Zeroizing::new(secret.to_string());
        run_db(self.path.clone(), move |connection| {
            let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let record = transaction
                .query_row(
                    "SELECT token_hash, edge_id, expires_at, consumed_at, revoked_at, replace_existing
                     FROM enrollments WHERE token_id = ?1",
                    [token_id.to_string()],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, Option<i64>>(3)?,
                            row.get::<_, Option<i64>>(4)?,
                            row.get::<_, bool>(5)?,
                        ))
                    },
                )
                .optional()?;
            let Some((token_hash, edge_id, expires_at, consumed_at, revoked_at, replace)) = record else {
                return Err(rusqlite::Error::QueryReturnedNoRows);
            };
            let now = unix_now();
            if edge_id != claimed_edge_id || expires_at <= now || consumed_at.is_some() || revoked_at.is_some() {
                return Err(rusqlite::Error::QueryReturnedNoRows);
            }
            let parsed_hash = PasswordHash::new(&token_hash).map_err(|_| rusqlite::Error::InvalidQuery)?;
            Argon2::default()
                .verify_password(secret.as_bytes(), &parsed_hash)
                .map_err(|_| rusqlite::Error::QueryReturnedNoRows)?;
            transaction.execute(
                "UPDATE enrollments SET consumed_at = ?2 WHERE token_id = ?1",
                params![token_id.to_string(), now],
            )?;
            transaction.commit()?;
            Ok(ConsumedEnrollment { token_id, edge_id, replace })
        })
        .await
        .map_err(|_| enrollment_error(GatewayErrorCode::IdentityRejected, "enrollment token rejected"))
    }

    pub async fn revoke(&self, token_id: Uuid) -> Result<(), GatewayError> {
        run_db(self.path.clone(), move |connection| {
            let changed = connection.execute(
                "UPDATE enrollments SET revoked_at = ?2 WHERE token_id = ?1 AND consumed_at IS NULL AND revoked_at IS NULL",
                params![token_id.to_string(), unix_now()],
            )?;
            if changed == 1 { Ok(()) } else { Err(rusqlite::Error::QueryReturnedNoRows) }
        })
        .await
        .map_err(|_| enrollment_error(GatewayErrorCode::RouteDenied, "enrollment token could not be revoked"))
    }
}

fn enrollment_error(code: GatewayErrorCode, message: impl Into<String>) -> GatewayError {
    GatewayError { code, message: message.into() }
}
