use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(version, about = "DBX Gateway certificate utility")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Init,
    Server,
    Client,
    Edge,
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
    fn pki_help_exposes_required_commands() {
        let command = Cli::command();
        let names = command.get_subcommands().map(|item| item.get_name()).collect::<Vec<_>>();

        assert_eq!(names, ["init", "server", "client", "edge"]);
        assert!(Cli::command().get_version().is_some());
        assert!(!Cli::command().render_long_help().to_string().is_empty());
    }
}
