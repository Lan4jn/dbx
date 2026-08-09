pub mod error;
pub mod protocol;

pub use error::{GatewayError, GatewayErrorCode};

pub fn command_not_implemented() -> Result<(), GatewayError> {
    Err(GatewayError { code: GatewayErrorCode::Internal, message: "command is not implemented".to_string() })
}
