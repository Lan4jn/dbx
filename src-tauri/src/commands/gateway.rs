use std::path::PathBuf;
use std::sync::Arc;

use dbx_core::connection::AppState;
use dbx_core::db::dbx_gateway::{GatewayEdgeRoutes, GatewayIdentityMetadata};
use dbx_core::models::connection::DbxGatewayConfig;
use tauri::State;

use crate::gateway_identity::KeyringGatewayIdentityProvider;

#[derive(Clone, Default)]
pub struct GatewayIdentityState(pub KeyringGatewayIdentityProvider);

#[tauri::command]
pub async fn import_gateway_identity(
    state: State<'_, Arc<AppState>>,
    identities: State<'_, GatewayIdentityState>,
    path: String,
    password: String,
    name: String,
) -> Result<GatewayIdentityMetadata, String> {
    let provider = identities.0.clone();
    let metadata =
        tauri::async_runtime::spawn_blocking(move || provider.import_pkcs12(&PathBuf::from(path), &password, &name))
            .await
            .map_err(|_| "Gateway identity import task failed".to_string())??;
    let mut current = state.storage.load_gateway_identity_metadata().await?;
    current.push(metadata.clone());
    if let Err(error) = state.storage.save_gateway_identity_metadata(&current).await {
        let provider = identities.0.clone();
        let identity_id = metadata.id.clone();
        let _ = tauri::async_runtime::spawn_blocking(move || provider.delete(&identity_id)).await;
        return Err(error);
    }
    Ok(metadata)
}

#[tauri::command]
pub async fn list_gateway_identities(state: State<'_, Arc<AppState>>) -> Result<Vec<GatewayIdentityMetadata>, String> {
    state.storage.load_gateway_identity_metadata().await
}

#[tauri::command]
pub async fn delete_gateway_identity(
    state: State<'_, Arc<AppState>>,
    identities: State<'_, GatewayIdentityState>,
    identity_id: String,
) -> Result<(), String> {
    let provider = identities.0.clone();
    let id = identity_id.clone();
    tauri::async_runtime::spawn_blocking(move || provider.delete(&id))
        .await
        .map_err(|_| "Gateway identity deletion task failed".to_string())??;
    let mut current = state.storage.load_gateway_identity_metadata().await?;
    current.retain(|identity| identity.id != identity_id);
    state.storage.save_gateway_identity_metadata(&current).await
}

#[tauri::command]
pub async fn list_gateway_routes(
    state: State<'_, Arc<AppState>>,
    profile: DbxGatewayConfig,
) -> Result<Vec<GatewayEdgeRoutes>, String> {
    state.dbx_gateway.list_routes(&profile).await
}

#[tauri::command]
pub async fn test_gateway_profile(
    state: State<'_, Arc<AppState>>,
    profile: DbxGatewayConfig,
) -> Result<String, String> {
    state.dbx_gateway.test_profile(&profile).await
}
