use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use dbx_core::models::connection::TransportLayerConfig;
use serde::Deserialize;

use crate::error::AppError;
use crate::state::WebState;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveTunnelProfilesRequest {
    pub profiles: Vec<TransportLayerConfig>,
}

pub async fn load_tunnel_profiles(
    State(state): State<Arc<WebState>>,
) -> Result<Json<Vec<TransportLayerConfig>>, AppError> {
    state.app.storage.load_tunnel_profiles().await.map(Json).map_err(AppError::from)
}

pub async fn save_tunnel_profiles(
    State(state): State<Arc<WebState>>,
    Json(body): Json<SaveTunnelProfilesRequest>,
) -> Result<Json<()>, AppError> {
    state.app.storage.save_tunnel_profiles(&body.profiles).await.map(Json).map_err(AppError::from)
}

pub async fn test_tunnel_profile(
    State(state): State<Arc<WebState>>,
    Json(profile): Json<TransportLayerConfig>,
) -> Result<Json<String>, AppError> {
    if matches!(profile, TransportLayerConfig::DbxGateway(_)) {
        return Err(AppError::bad_request("DBX Gateway profiles can only be tested in the desktop app."));
    }
    state.app.test_tunnel_profile(&profile).await.map(Json).map_err(AppError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use dbx_core::connection::AppState;
    use dbx_core::models::connection::DbxGatewayConfig;
    use dbx_core::storage::Storage;

    #[tokio::test]
    async fn gateway_profile_test_is_limited_to_the_desktop_app() {
        let dir = std::env::temp_dir().join(format!("dbx-web-gateway-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let storage = Storage::open(&dir.join("storage.db")).await.unwrap();
        let app = Arc::new(AppState::new_with_plugin_dir(storage, dir.join("plugins")));
        let state = Arc::new(WebState::for_tests(app, dir.clone()));
        let profile = TransportLayerConfig::DbxGateway(DbxGatewayConfig {
            id: "gateway-1".to_string(),
            name: "Gateway".to_string(),
            enabled: true,
            profile_id: String::new(),
            main_url: "wss://gateway.example.com/dbx".to_string(),
            identity_id: "identity-1".to_string(),
            server_ca_pem: "test-ca".to_string(),
            server_spki_sha256: "a".repeat(64),
            connect_timeout_secs: 10,
            edge_id: String::new(),
            target_id: String::new(),
            use_as_connection_info: true,
        });

        let error = test_tunnel_profile(State(state), Json(profile)).await.unwrap_err();

        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert_eq!(error.message, "DBX Gateway profiles can only be tested in the desktop app.");
        let _ = std::fs::remove_dir_all(dir);
    }
}
