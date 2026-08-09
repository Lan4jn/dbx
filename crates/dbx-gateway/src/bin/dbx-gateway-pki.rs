use std::fs;
use std::fs::File;
use std::io::Read;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::str::FromStr;

use clap::{Args, Parser, Subcommand};
use dbx_gateway::pki::{
    write_output_file, CertificateRole, ClientIssueRequest, EdgeIssueRequest, PkiStore, RevocationReason,
    ServerIssueRequest,
};
use dbx_gateway::GatewayError;
use zeroize::Zeroizing;

#[derive(Debug, Parser)]
#[command(version, about = "DBX Gateway certificate utility")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Init(InitArgs),
    Server(ServerCommand),
    Client(ClientCommand),
    Edge(EdgeCommand),
}

#[derive(Debug, Args)]
struct InitArgs {
    #[arg(long)]
    data_dir: PathBuf,
    #[arg(long)]
    password_file: PathBuf,
}

#[derive(Debug, Args)]
struct ServerCommand {
    #[command(subcommand)]
    command: ServerAction,
}

#[derive(Debug, Subcommand)]
enum ServerAction {
    Issue(ServerIssueArgs),
    Renew(ServerIssueArgs),
    Revoke(RevokeArgs),
}

#[derive(Debug, Args)]
struct ServerIssueArgs {
    #[arg(long)]
    data_dir: PathBuf,
    #[arg(long)]
    password_file: PathBuf,
    #[arg(long)]
    identity: String,
    #[arg(long = "dns-san", required = true)]
    dns_sans: Vec<String>,
    #[arg(long = "ip-san")]
    ip_sans: Vec<IpAddr>,
    #[arg(long)]
    output_dir: PathBuf,
}

#[derive(Debug, Args)]
struct ClientCommand {
    #[command(subcommand)]
    command: ClientAction,
}

#[derive(Debug, Subcommand)]
enum ClientAction {
    Issue(ClientIssueArgs),
    Renew(ClientIssueArgs),
    Revoke(RevokeArgs),
}

#[derive(Debug, Args)]
struct ClientIssueArgs {
    #[arg(long)]
    data_dir: PathBuf,
    #[arg(long)]
    password_file: PathBuf,
    #[arg(long)]
    bundle_password_file: PathBuf,
    #[arg(long)]
    identity: String,
    #[arg(long)]
    output_dir: PathBuf,
}

#[derive(Debug, Args)]
struct EdgeCommand {
    #[command(subcommand)]
    command: EdgeAction,
}

#[derive(Debug, Subcommand)]
enum EdgeAction {
    Issue(EdgeIssueArgs),
    Renew(EdgeIssueArgs),
    Revoke(RevokeArgs),
}

#[derive(Debug, Args)]
struct EdgeIssueArgs {
    #[arg(long)]
    data_dir: PathBuf,
    #[arg(long)]
    password_file: PathBuf,
    #[arg(long)]
    identity: String,
    #[arg(long)]
    csr: PathBuf,
    #[arg(long)]
    output_dir: PathBuf,
}

#[derive(Debug, Args)]
struct RevokeArgs {
    #[arg(long)]
    data_dir: PathBuf,
    #[arg(long)]
    password_file: PathBuf,
    #[arg(long)]
    serial: String,
    #[arg(long, default_value = "unspecified")]
    reason: String,
}

fn main() -> ExitCode {
    match dispatch(Cli::parse()) {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{}", error.message);
            ExitCode::FAILURE
        }
    }
}

fn dispatch(cli: Cli) -> Result<String, GatewayError> {
    match cli.command {
        Command::Init(args) => {
            let password = read_password_file(&args.password_file)?;
            PkiStore::init(&args.data_dir, &password)?;
            Ok("initialized DBX Gateway PKI".to_string())
        }
        Command::Server(command) => match command.command {
            ServerAction::Issue(args) | ServerAction::Renew(args) => issue_server(args),
            ServerAction::Revoke(args) => revoke(CertificateRole::Server, args),
        },
        Command::Client(command) => match command.command {
            ClientAction::Issue(args) | ClientAction::Renew(args) => issue_client(args),
            ClientAction::Revoke(args) => revoke(CertificateRole::Client, args),
        },
        Command::Edge(command) => match command.command {
            EdgeAction::Issue(args) | EdgeAction::Renew(args) => issue_edge(args),
            EdgeAction::Revoke(args) => revoke(CertificateRole::Edge, args),
        },
    }
}

fn issue_server(args: ServerIssueArgs) -> Result<String, GatewayError> {
    let password = read_password_file(&args.password_file)?;
    prepare_output_dir(&args.output_dir)?;
    let store = PkiStore::open(&args.data_dir)?;
    let dns_sans = args.dns_sans.iter().map(String::as_str).collect::<Vec<_>>();
    let issued = store.issue_server(
        ServerIssueRequest {
            name: &args.identity,
            dns_sans: &dns_sans,
            ip_sans: &args.ip_sans,
            validity: time::Duration::days(365),
        },
        &password,
    )?;
    write_output_file(&args.output_dir.join("certificate.pem"), issued.issued.certificate_pem.as_bytes(), 0o644)?;
    write_output_file(&args.output_dir.join("chain.pem"), issued.issued.chain_pem.as_bytes(), 0o644)?;
    write_output_file(&args.output_dir.join("private-key.pem"), issued.private_key_pem.as_bytes(), 0o600)?;
    Ok(format!("issued server certificate {}", issued.issued.serial_hex))
}

fn issue_client(args: ClientIssueArgs) -> Result<String, GatewayError> {
    let password = read_password_file(&args.password_file)?;
    let bundle_password = read_password_file(&args.bundle_password_file)?;
    prepare_output_dir(&args.output_dir)?;
    let store = PkiStore::open(&args.data_dir)?;
    let issued = store.issue_client(
        ClientIssueRequest {
            client_id: &args.identity,
            validity: time::Duration::days(365),
            bundle_password: &bundle_password,
        },
        &password,
    )?;
    write_output_file(
        &args.output_dir.join("certificate.pem"),
        issued.key_pair.issued.certificate_pem.as_bytes(),
        0o644,
    )?;
    write_output_file(&args.output_dir.join("chain.pem"), issued.key_pair.issued.chain_pem.as_bytes(), 0o644)?;
    write_output_file(&args.output_dir.join("private-key.pem"), issued.key_pair.private_key_pem.as_bytes(), 0o600)?;
    write_output_file(&args.output_dir.join("client.p12"), &issued.pkcs12_der, 0o600)?;
    Ok(format!("issued client certificate {}", issued.key_pair.issued.serial_hex))
}

fn issue_edge(args: EdgeIssueArgs) -> Result<String, GatewayError> {
    let password = read_password_file(&args.password_file)?;
    let csr = fs::read(&args.csr).map_err(|_| pki_error("could not read edge CSR"))?;
    prepare_output_dir(&args.output_dir)?;
    let store = PkiStore::open(&args.data_dir)?;
    let issued = store.issue_edge(
        EdgeIssueRequest { edge_id: &args.identity, csr_der: &csr, validity: time::Duration::days(365) },
        &password,
    )?;
    write_output_file(&args.output_dir.join("certificate.pem"), issued.certificate_pem.as_bytes(), 0o644)?;
    write_output_file(&args.output_dir.join("chain.pem"), issued.chain_pem.as_bytes(), 0o644)?;
    Ok(format!("issued edge certificate {}", issued.serial_hex))
}

fn revoke(role: CertificateRole, args: RevokeArgs) -> Result<String, GatewayError> {
    let password = read_password_file(&args.password_file)?;
    let reason = RevocationReason::from_str(&args.reason)?;
    let store = PkiStore::open(&args.data_dir)?;
    let crl = store.revoke(role, &args.serial, reason, &password)?;
    Ok(format!("revoked {role} certificate; CRL number {}", crl.number))
}

fn read_password_file(path: &Path) -> Result<Zeroizing<String>, GatewayError> {
    let path_metadata = fs::symlink_metadata(path).map_err(|_| pki_error("could not read password file"))?;
    if path_metadata.file_type().is_symlink() || !path_metadata.file_type().is_file() {
        return Err(pki_error("password file must be a real regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if path_metadata.permissions().mode() & 0o7777 != 0o600 {
            return Err(pki_error("password file permissions must be 0600"));
        }
    }
    let mut file = File::open(path).map_err(|_| pki_error("could not read password file"))?;
    let opened_metadata = file.metadata().map_err(|_| pki_error("could not inspect password file"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if path_metadata.dev() != opened_metadata.dev() || path_metadata.ino() != opened_metadata.ino() {
            return Err(pki_error("password file changed while opening"));
        }
    }
    let mut password = String::new();
    file.read_to_string(&mut password).map_err(|_| pki_error("could not read password file"))?;
    if let Some(stripped) = password.strip_suffix("\r\n") {
        password.truncate(stripped.len());
    } else if password.ends_with('\n') {
        password.pop();
    }
    if password.is_empty() {
        return Err(pki_error("password file must not be empty"));
    }
    Ok(Zeroizing::new(password))
}

fn prepare_output_dir(path: &Path) -> Result<(), GatewayError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(pki_error("output path must be a real directory"));
            }
            if fs::read_dir(path).map_err(|_| pki_error("could not inspect output directory"))?.next().is_some() {
                return Err(pki_error("output directory must be empty"));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path).map_err(|_| pki_error("could not create output directory"))?;
        }
        Err(_) => return Err(pki_error("could not inspect output directory")),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|_| pki_error("could not secure output directory"))?;
    }
    Ok(())
}

fn pki_error(message: &str) -> GatewayError {
    GatewayError { code: dbx_gateway::GatewayErrorCode::Internal, message: message.to_string() }
}

#[cfg(test)]
mod tests {
    use super::{read_password_file, Cli};
    use clap::CommandFactory;

    #[test]
    fn pki_help_exposes_nested_role_commands_without_plaintext_passwords() {
        let command = Cli::command();
        let names = command.get_subcommands().map(|item| item.get_name()).collect::<Vec<_>>();

        assert_eq!(names, ["init", "server", "client", "edge"]);
        let command = Cli::command();
        for role in ["server", "client", "edge"] {
            let role_command = command.find_subcommand(role).unwrap();
            let actions = role_command.get_subcommands().map(|item| item.get_name()).collect::<Vec<_>>();
            assert_eq!(actions, ["issue", "renew", "revoke"]);
        }
        assert!(Cli::command().get_version().is_some());
        assert_no_plaintext_password_argument(&Cli::command());
    }

    #[cfg(unix)]
    #[test]
    fn password_files_must_not_be_symbolic_links() {
        use std::fs;
        use std::os::unix::fs::{symlink, PermissionsExt};

        let directory = std::env::temp_dir().join(format!(
            "dbx-gateway-password-test-{}-{}",
            std::process::id(),
            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let target = directory.join("password.txt");
        fs::write(&target, "secret").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        let link = directory.join("password-link.txt");
        symlink(&target, &link).unwrap();

        assert!(read_password_file(&link).is_err());
        fs::remove_dir_all(directory).unwrap();
    }

    fn assert_no_plaintext_password_argument(command: &clap::Command) {
        for argument in command.get_arguments() {
            assert_ne!(argument.get_id().as_str(), "password");
            assert_ne!(argument.get_long(), Some("password"));
        }
        for subcommand in command.get_subcommands() {
            assert_no_plaintext_password_argument(subcommand);
        }
    }
}
