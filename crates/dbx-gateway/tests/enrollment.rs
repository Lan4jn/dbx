#![cfg(feature = "server")]

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use dbx_gateway::state::GatewayState;

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        loop {
            let id = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("dbx-gateway-enrollment-{}-{id}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("failed to create test directory: {error}"),
            }
        }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

async fn test_state() -> (TempDir, GatewayState) {
    let dir = TempDir::new();
    let state = GatewayState::open(dir.0.join("state.sqlite3")).await.unwrap();
    (dir, state)
}

#[tokio::test]
async fn token_database_stores_only_an_argon2id_hash() {
    let (_dir, state) = test_state().await;
    let token = state.enrollments.create("edge-prod-01", Duration::from_secs(600), false).await.unwrap();
    let bytes = fs::read(state.path()).unwrap();

    assert!(!bytes.windows(token.secret.len()).any(|window| window == token.secret.as_bytes()));
    assert!(bytes.windows(10).any(|window| window == b"$argon2id$"));
    assert!(token.expires_at - token.created_at >= time::Duration::minutes(9));
}

#[tokio::test]
async fn token_is_edge_bound_and_consumed_once_under_concurrency() {
    let (_dir, state) = test_state().await;
    let token = state.enrollments.create("edge-prod-01", Duration::from_secs(600), false).await.unwrap();
    assert!(state.enrollments.consume("edge-prod-02", &token.secret).await.is_err());

    let results = futures_util::future::join_all((0..20).map(|_| {
        let enrollments = state.enrollments.clone();
        let secret = token.secret.to_string();
        async move { enrollments.consume("edge-prod-01", &secret).await }
    }))
    .await;

    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
}

#[tokio::test]
async fn token_revocation_prevents_consumption() {
    let (_dir, state) = test_state().await;
    let token = state.enrollments.create("edge-prod-01", Duration::from_secs(600), false).await.unwrap();
    state.enrollments.revoke(token.id).await.unwrap();

    assert!(state.enrollments.consume("edge-prod-01", &token.secret).await.is_err());
}

#[tokio::test]
async fn token_expiration_prevents_consumption() {
    let (_dir, state) = test_state().await;
    let token = state.enrollments.create("edge-prod-01", Duration::from_secs(1), false).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;

    assert!(state.enrollments.consume("edge-prod-01", &token.secret).await.is_err());
}

#[tokio::test]
async fn token_replace_is_required_for_an_edge_with_an_active_certificate() {
    let (_dir, state) = test_state().await;
    state.record_issued_certificate("edge-prod-01", "01AB").await.unwrap();

    assert!(state.enrollments.create("edge-prod-01", Duration::from_secs(600), false).await.is_err());
    let token = state.enrollments.create("edge-prod-01", Duration::from_secs(600), true).await.unwrap();

    assert!(token.replace);
    assert!(state.certificate_is_revoked("01AB").await.unwrap());
}
