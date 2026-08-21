use std::path::PathBuf;
use std::sync::Arc;

use dbx_core::connection::AppState;
use dbx_core::db::dbx_gateway::{GatewayEdgeRoutes, GatewayIdentityMetadata};
use dbx_core::models::connection::DbxGatewayConfig;
use tauri::State;
use zeroize::Zeroizing;

use crate::gateway_identity::StorageGatewayIdentityProvider;

#[derive(Clone)]
pub struct GatewayIdentityState(pub Arc<StorageGatewayIdentityProvider>);

#[tauri::command]
pub async fn import_gateway_identity(
    _state: State<'_, Arc<AppState>>,
    identities: State<'_, GatewayIdentityState>,
    path: String,
    password: String,
    name: String,
) -> Result<GatewayIdentityMetadata, String> {
    let path = PathBuf::from(path);
    let password = Zeroizing::new(password);
    identities.0.import_pkcs12(&path, &password, &name).await
}

#[tauri::command]
pub async fn list_gateway_identities(state: State<'_, Arc<AppState>>) -> Result<Vec<GatewayIdentityMetadata>, String> {
    state.storage.load_gateway_identity_metadata().await
}

#[tauri::command]
pub async fn delete_gateway_identity(
    _state: State<'_, Arc<AppState>>,
    identities: State<'_, GatewayIdentityState>,
    identity_id: String,
) -> Result<(), String> {
    identities.0.delete(&identity_id).await
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
