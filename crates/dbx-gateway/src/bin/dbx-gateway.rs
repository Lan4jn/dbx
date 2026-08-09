use std::path::PathBuf;

use clap::{Parser, Subcommand};

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

fn main() -> Result<(), dbx_gateway::GatewayError> {
    let _cli = Cli::parse();
    dbx_gateway::command_not_implemented()
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
