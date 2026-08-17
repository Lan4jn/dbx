#[cfg(feature = "server")]
pub mod config;
#[cfg(feature = "server")]
pub mod edge_gateway;
#[cfg(feature = "server")]
pub mod enrollment;
pub mod error;
#[cfg(feature = "server")]
pub mod health;
#[cfg(feature = "server")]
pub mod limits;
#[cfg(feature = "server")]
pub mod main_gateway;
#[cfg(feature = "server")]
pub mod pki;
pub mod protocol;
#[cfg(feature = "server")]
pub mod reverse_proxy;
#[cfg(feature = "server")]
pub mod state;
#[cfg(feature = "server")]
pub mod stream;
#[cfg(feature = "server")]
pub mod tls;

pub use error::{GatewayError, GatewayErrorCode};

#[cfg(feature = "server")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatewayCommand {
    Serve,
    CheckConfig,
}

#[cfg(feature = "server")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayCommandResult {
    pub exit_code: u8,
    pub message: String,
}

#[cfg(feature = "server")]
pub async fn run_gateway_command(command: GatewayCommand, config_path: &std::path::Path) -> GatewayCommandResult {
    match dispatch_gateway_command(command, config_path).await {
        Ok(message) => GatewayCommandResult { exit_code: 0, message: message.to_string() },
        Err(error) => GatewayCommandResult {
            exit_code: if error.code == GatewayErrorCode::ConfigInvalid { 2 } else { 1 },
            message: error.message,
        },
    }
}

#[cfg(feature = "server")]
async fn dispatch_gateway_command(
    command: GatewayCommand,
    config_path: &std::path::Path,
) -> Result<&'static str, GatewayError> {
    let config = config::load_config_file(config_path)?;
    match command {
        GatewayCommand::CheckConfig => Ok("configuration is valid"),
        GatewayCommand::Serve => {
            match config {
                config::GatewayConfig::Main(config) => {
                    let gateway = main_gateway::MainGateway::bind(*config).await?;
                    loop {
                        match wait_for_gateway_signal().await? {
                            GatewaySignal::Shutdown => {
                                gateway.shutdown().await;
                                break;
                            }
                            GatewaySignal::Reload => {
                                if let Ok(config::GatewayConfig::Main(config)) = config::load_config_file(config_path) {
                                    let _ = gateway.reload(*config).await;
                                }
                            }
                        }
                    }
                }
                config::GatewayConfig::Edge(config) => {
                    let mut gateway = edge_gateway::EdgeGateway::start(*config)?;
                    loop {
                        match wait_for_gateway_signal().await? {
                            GatewaySignal::Shutdown => {
                                gateway.shutdown().await;
                                break;
                            }
                            GatewaySignal::Reload => {
                                if let Ok(config::GatewayConfig::Edge(config)) = config::load_config_file(config_path) {
                                    if let Ok(reloaded) = edge_gateway::EdgeGateway::start(*config) {
                                        gateway.shutdown().await;
                                        gateway = reloaded;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Ok("gateway stopped")
        }
    }
}

#[cfg(feature = "server")]
enum GatewaySignal {
    Shutdown,
    Reload,
}

#[cfg(feature = "server")]
async fn wait_for_gateway_signal() -> Result<GatewaySignal, GatewayError> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        let mut terminate = signal(SignalKind::terminate()).map_err(|_| signal_error())?;
        let mut reload = signal(SignalKind::hangup()).map_err(|_| signal_error())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result.map(|_| GatewaySignal::Shutdown).map_err(|_| signal_error()),
            _ = terminate.recv() => Ok(GatewaySignal::Shutdown),
            _ = reload.recv() => Ok(GatewaySignal::Reload),
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await.map(|_| GatewaySignal::Shutdown).map_err(|_| signal_error())
}

#[cfg(feature = "server")]
fn signal_error() -> GatewayError {
    GatewayError { code: GatewayErrorCode::Internal, message: "shutdown signal handler could not start".to_string() }
}
