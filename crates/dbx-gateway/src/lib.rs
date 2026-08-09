#[cfg(feature = "server")]
pub mod config;
pub mod error;
pub mod protocol;

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
pub fn run_gateway_command(command: GatewayCommand, config_path: &std::path::Path) -> GatewayCommandResult {
    match dispatch_gateway_command(command, config_path) {
        Ok(message) => GatewayCommandResult { exit_code: 0, message: message.to_string() },
        Err(error) => GatewayCommandResult {
            exit_code: if error.code == GatewayErrorCode::ConfigInvalid { 2 } else { 1 },
            message: error.message,
        },
    }
}

#[cfg(feature = "server")]
fn dispatch_gateway_command(
    command: GatewayCommand,
    config_path: &std::path::Path,
) -> Result<&'static str, GatewayError> {
    config::load_config_file(config_path)?;
    match command {
        GatewayCommand::CheckConfig => Ok("configuration is valid"),
        GatewayCommand::Serve => {
            command_not_implemented()?;
            unreachable!()
        }
    }
}

pub fn command_not_implemented() -> Result<(), GatewayError> {
    Err(GatewayError { code: GatewayErrorCode::Internal, message: "command is not implemented".to_string() })
}
