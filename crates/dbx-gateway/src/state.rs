use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};

use crate::enrollment::EnrollmentStore;
use crate::protocol::RegisteredTarget;
use crate::{GatewayError, GatewayErrorCode};

#[derive(Clone)]
pub struct GatewayState {
    path: PathBuf,
    pub enrollments: EnrollmentStore,
}

impl GatewayState {
    pub async fn open(path: PathBuf) -> Result<Self, GatewayError> {
        let database_path = path.clone();
        run_db(path.clone(), move |connection| {
            connection.execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA foreign_keys = ON;
                 CREATE TABLE IF NOT EXISTS enrollments (
                    token_id TEXT PRIMARY KEY,
                    token_hash TEXT NOT NULL,
                    edge_id TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    expires_at INTEGER NOT NULL,
                    consumed_at INTEGER,
                    revoked_at INTEGER,
                    replace_existing INTEGER NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS issued_certificates (
                    serial_hex TEXT PRIMARY KEY,
                    edge_id TEXT NOT NULL,
                    issued_at INTEGER NOT NULL,
                    revoked_at INTEGER
                 );
                 CREATE TABLE IF NOT EXISTS revocations (
                    serial_hex TEXT PRIMARY KEY,
                    edge_id TEXT NOT NULL,
                    revoked_at INTEGER NOT NULL,
                    reason TEXT NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS edge_routes (
                    edge_id TEXT NOT NULL,
                    target_id TEXT NOT NULL,
                    display_name TEXT NOT NULL,
                    PRIMARY KEY (edge_id, target_id)
                 );",
            )?;
            Ok(())
        })
        .await?;
        Ok(Self { path: database_path.clone(), enrollments: EnrollmentStore::new(database_path) })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn record_issued_certificate(&self, edge_id: &str, serial_hex: &str) -> Result<(), GatewayError> {
        let edge_id = edge_id.to_string();
        let serial_hex = serial_hex.to_string();
        run_db(self.path.clone(), move |connection| {
            connection.execute(
                "INSERT INTO issued_certificates (serial_hex, edge_id, issued_at, revoked_at) VALUES (?1, ?2, ?3, NULL)",
                params![serial_hex, edge_id, unix_now()],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn certificate_is_revoked(&self, serial_hex: &str) -> Result<bool, GatewayError> {
        let serial_hex = serial_hex.to_string();
        run_db(self.path.clone(), move |connection| {
            connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM revocations WHERE serial_hex = ?1)",
                [serial_hex],
                |row| row.get(0),
            )
        })
        .await
    }

    pub async fn certificate_is_active(&self, edge_id: &str, serial_hex: &str) -> Result<bool, GatewayError> {
        let edge_id = edge_id.to_string();
        let serial_hex = normalized_serial(serial_hex);
        run_db(self.path.clone(), move |connection| {
            connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM issued_certificates
                 WHERE edge_id = ?1 AND lower(serial_hex) = ?2 AND revoked_at IS NULL)",
                params![edge_id, serial_hex],
                |row| row.get(0),
            )
        })
        .await
    }

    pub async fn rotate_issued_certificate(
        &self,
        edge_id: &str,
        previous_serial: &str,
        new_serial: &str,
    ) -> Result<(), GatewayError> {
        let edge_id = edge_id.to_string();
        let previous_serial = normalized_serial(previous_serial);
        let new_serial = new_serial.to_ascii_lowercase();
        run_db(self.path.clone(), move |connection| {
            let transaction = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let now = unix_now();
            let changed = transaction.execute(
                "UPDATE issued_certificates SET revoked_at = ?3
                 WHERE edge_id = ?1 AND lower(serial_hex) = ?2 AND revoked_at IS NULL",
                params![edge_id, previous_serial, now],
            )?;
            if changed != 1 {
                return Err(rusqlite::Error::QueryReturnedNoRows);
            }
            transaction.execute(
                "INSERT INTO revocations (serial_hex, edge_id, revoked_at, reason) VALUES (?1, ?2, ?3, 'superseded')",
                params![previous_serial, edge_id, now],
            )?;
            transaction.execute(
                "INSERT INTO issued_certificates (serial_hex, edge_id, issued_at, revoked_at) VALUES (?1, ?2, ?3, NULL)",
                params![new_serial, edge_id, now],
            )?;
            transaction.commit()
        })
        .await
    }

    pub async fn revocation_count_for_edge(&self, edge_id: &str) -> Result<u64, GatewayError> {
        let edge_id = edge_id.to_string();
        run_db(self.path.clone(), move |connection| {
            connection.query_row("SELECT COUNT(*) FROM revocations WHERE edge_id = ?1", [edge_id], |row| row.get(0))
        })
        .await
    }

    pub async fn replace_edge_routes(
        &self,
        edge_id: &str,
        targets: &BTreeMap<String, RegisteredTarget>,
    ) -> Result<(), GatewayError> {
        let edge_id = edge_id.to_string();
        let targets =
            targets.values().map(|target| (target.target_id.clone(), target.display_name.clone())).collect::<Vec<_>>();
        run_db(self.path.clone(), move |connection| {
            let transaction = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            transaction.execute("DELETE FROM edge_routes WHERE edge_id = ?1", [&edge_id])?;
            for (target_id, display_name) in targets {
                transaction.execute(
                    "INSERT INTO edge_routes (edge_id, target_id, display_name) VALUES (?1, ?2, ?3)",
                    params![edge_id, target_id, display_name],
                )?;
            }
            transaction.commit()
        })
        .await
    }

    pub async fn load_edge_routes(&self) -> Result<BTreeMap<String, BTreeMap<String, RegisteredTarget>>, GatewayError> {
        run_db(self.path.clone(), move |connection| {
            let mut statement = connection
                .prepare("SELECT edge_id, target_id, display_name FROM edge_routes ORDER BY edge_id, target_id")?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
            })?;
            let mut edges = BTreeMap::<String, BTreeMap<String, RegisteredTarget>>::new();
            for row in rows {
                let (edge_id, target_id, display_name) = row?;
                edges
                    .entry(edge_id)
                    .or_default()
                    .insert(target_id.clone(), RegisteredTarget { target_id, display_name });
            }
            Ok(edges)
        })
        .await
    }
}

pub(crate) async fn run_db<T, F>(path: PathBuf, operation: F) -> Result<T, GatewayError>
where
    T: Send + 'static,
    F: FnOnce(&mut Connection) -> Result<T, rusqlite::Error> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let mut connection = Connection::open(path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        operation(&mut connection)
    })
    .await
    .map_err(|_| state_error("gateway state operation failed"))?
    .map_err(|_| state_error("gateway state operation failed"))
}

pub(crate) fn unix_now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

pub(crate) fn state_error(message: impl Into<String>) -> GatewayError {
    GatewayError { code: GatewayErrorCode::Internal, message: message.into() }
}

fn normalized_serial(serial: &str) -> String {
    serial.chars().filter(|character| character.is_ascii_hexdigit()).flat_map(char::to_lowercase).collect()
}
