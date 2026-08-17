use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use dbx_gateway::{run_gateway_command, GatewayCommand};

#[derive(Debug, Parser)]
#[command(version, about = "DBX database gateway")]
struct Cli {
    #[arg(long, global = true, default_value = "dbx-gateway.toml")]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Serve,
    CheckConfig,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let command = match cli.command {
        Command::Serve => GatewayCommand::Serve,
        Command::CheckConfig => GatewayCommand::CheckConfig,
    };
    let result = run_gateway_command(command, &cli.config).await;
    if result.exit_code == 0 {
        println!("{}", result.message);
    } else {
        eprintln!("{}", result.message);
    }
    ExitCode::from(result.exit_code)
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::CommandFactory;

    #[test]
    fn gateway_help_exposes_required_commands() {
        let command = Cli::command();
        let names = command.get_subcommands().map(|item| item.get_name()).collect::<Vec<_>>();

        assert_eq!(names, ["serve", "check-config"]);
        assert!(Cli::command().get_version().is_some());
        assert!(Cli::command().render_long_help().to_string().contains("--config"));
    }
}
