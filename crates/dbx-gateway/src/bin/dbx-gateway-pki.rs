use std::fs;
use std::fs::File;
use std::io::{IsTerminal, Read, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::str::FromStr;

use clap::{Args, Parser, Subcommand};
use dbx_gateway::pki::{
    serve_remote, serve_unix, write_output_file, CertificateRole, ClientIssueRequest, EdgeIssueRequest,
    PkiEnrollmentService, PkiStore, RemotePkiConfig, RevocationReason, ServerIssueRequest,
};
use dbx_gateway::state::GatewayState;
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
    Serve(ServeArgs),
    Init(InitArgs),
    Server(ServerCommand),
    Client(ClientCommand),
    Edge(EdgeCommand),
    Enrollment(EnrollmentCommand),
}

#[derive(Debug, Args)]
struct ServeArgs {
    #[arg(long)]
    config: PathBuf,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ServiceFileConfig {
    data_dir: PathBuf,
    password_file: PathBuf,
    #[serde(default = "default_state_file")]
    state_file: PathBuf,
    unix: Option<UnixServiceConfig>,
    remote: Option<RemoteServiceConfig>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct UnixServiceConfig {
    path: PathBuf,
    allowed_uid: u32,
    allowed_gid: u32,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteServiceConfig {
    listen: String,
    certificate: PathBuf,
    private_key: PathBuf,
    main_ra_ca_certificate: PathBuf,
    allowed_ra_uri_sans: Vec<String>,
}

fn default_state_file() -> PathBuf {
    PathBuf::from("gateway-state.sqlite3")
}

#[derive(Debug, Args)]
struct EnrollmentCommand {
    #[command(subcommand)]
    command: EnrollmentAction,
}

#[derive(Debug, Subcommand)]
enum EnrollmentAction {
    Create(EnrollmentCreateArgs),
    Revoke(EnrollmentRevokeArgs),
}

#[derive(Debug, Args)]
struct EnrollmentCreateArgs {
    #[arg(long)]
    data_dir: PathBuf,
    #[arg(long)]
    edge_id: String,
    #[arg(long, default_value = "10m", value_parser = parse_ttl)]
    ttl: std::time::Duration,
    #[arg(long)]
    replace: bool,
    #[arg(long, requires = "replace")]
    yes: bool,
}

#[derive(Debug, Args)]
struct EnrollmentRevokeArgs {
    #[arg(long)]
    data_dir: PathBuf,
    #[arg(long)]
    token_id: uuid::Uuid,
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
    #[arg(long = "dns-san")]
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

#[tokio::main]
async fn main() -> ExitCode {
    match dispatch(Cli::parse()).await {
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

async fn dispatch(cli: Cli) -> Result<String, GatewayError> {
    match cli.command {
        Command::Serve(args) => serve(args).await,
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
        Command::Enrollment(command) => match command.command {
            EnrollmentAction::Create(args) => create_enrollment(args).await,
            EnrollmentAction::Revoke(args) => revoke_enrollment(args).await,
        },
    }
}

async fn serve(args: ServeArgs) -> Result<String, GatewayError> {
    let config_path = fs::canonicalize(&args.config).map_err(|_| pki_error("PKI service config could not be read"))?;
    let contents = fs::read_to_string(&config_path).map_err(|_| pki_error("PKI service config could not be read"))?;
    let mut config: ServiceFileConfig =
        toml::from_str(&contents).map_err(|_| pki_error("PKI service config is invalid"))?;
    if config.unix.is_none() && config.remote.is_none() {
        return Err(pki_error("PKI service requires a Unix or remote listener"));
    }
    let base = config_path.parent().unwrap_or_else(|| Path::new("."));
    resolve_service_path(&mut config.data_dir, base);
    resolve_service_path(&mut config.password_file, base);
    resolve_service_path(&mut config.state_file, base);
    if let Some(unix) = &mut config.unix {
        resolve_service_path(&mut unix.path, base);
    }
    if let Some(remote) = &mut config.remote {
        resolve_service_path(&mut remote.certificate, base);
        resolve_service_path(&mut remote.private_key, base);
        resolve_service_path(&mut remote.main_ra_ca_certificate, base);
    }

    let password = read_password_file(&config.password_file)?;
    let state = GatewayState::open(config.state_file).await?;
    let store = PkiStore::open_online_edge(&config.data_dir)?;
    let service = PkiEnrollmentService::new(state, store, password);
    let unix = match config.unix {
        Some(unix) => Some(serve_unix(unix.path, unix.allowed_uid, unix.allowed_gid, service.clone()).await?),
        None => None,
    };
    let remote = match config.remote {
        Some(remote) => Some(
            serve_remote(
                RemotePkiConfig {
                    listen: remote.listen,
                    certificate: remote.certificate,
                    private_key: remote.private_key,
                    main_ra_ca_certificate: remote.main_ra_ca_certificate,
                    allowed_ra_uri_sans: remote.allowed_ra_uri_sans,
                },
                service,
            )
            .await?,
        ),
        None => None,
    };
    wait_for_shutdown_signal().await?;
    if let Some(server) = unix {
        server.shutdown().await;
    }
    if let Some(server) = remote {
        server.shutdown().await;
    }
    Ok("PKI service stopped".to_string())
}

fn resolve_service_path(path: &mut PathBuf, base: &Path) {
    if path.is_relative() {
        *path = base.join(&*path);
    }
}

async fn wait_for_shutdown_signal() -> Result<(), GatewayError> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut terminate = signal(SignalKind::terminate()).map_err(|_| pki_error("signal handler failed"))?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result.map_err(|_| pki_error("signal handler failed")),
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await.map_err(|_| pki_error("signal handler failed"))
}

async fn create_enrollment(args: EnrollmentCreateArgs) -> Result<String, GatewayError> {
    if args.replace && !args.yes {
        confirm_replace(&args.edge_id)?;
    }
    let state = GatewayState::open(args.data_dir.join("gateway-state.sqlite3")).await?;
    let token = state.enrollments.create(&args.edge_id, args.ttl, args.replace).await?;
    Ok(format!(
        "enrollment token {} for {} expires at {}\n{}",
        token.id,
        token.edge_id,
        token.expires_at,
        token.secret.as_str()
    ))
}

async fn revoke_enrollment(args: EnrollmentRevokeArgs) -> Result<String, GatewayError> {
    let state = GatewayState::open(args.data_dir.join("gateway-state.sqlite3")).await?;
    state.enrollments.revoke(args.token_id).await?;
    Ok(format!("revoked enrollment token {}", args.token_id))
}

fn confirm_replace(edge_id: &str) -> Result<(), GatewayError> {
    if !std::io::stdin().is_terminal() {
        return Err(pki_error("--replace requires interactive confirmation or --yes"));
    }
    eprint!("Replace the active certificate for Edge {edge_id}? [y/N] ");
    std::io::stderr().flush().map_err(|_| pki_error("could not request confirmation"))?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer).map_err(|_| pki_error("could not read confirmation"))?;
    if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        Ok(())
    } else {
        Err(pki_error("replacement cancelled"))
    }
}

fn parse_ttl(value: &str) -> Result<std::time::Duration, String> {
    let (number, multiplier) = match value.chars().last() {
        Some('s') => (&value[..value.len() - 1], 1_u64),
        Some('m') => (&value[..value.len() - 1], 60),
        Some('h') => (&value[..value.len() - 1], 60 * 60),
        _ => (value, 1),
    };
    let amount = number.parse::<u64>().map_err(|_| "TTL must be a positive duration such as 10m".to_string())?;
    let seconds = amount
        .checked_mul(multiplier)
        .filter(|seconds| *seconds > 0)
        .ok_or_else(|| "TTL must be a positive duration such as 10m".to_string())?;
    Ok(std::time::Duration::from_secs(seconds))
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
    use super::{read_password_file, Cli, Command, EnrollmentAction, ServerAction};
    use clap::{CommandFactory, Parser};

    #[test]
    fn pki_help_exposes_nested_role_commands_without_plaintext_passwords() {
        let command = Cli::command();
        let names = command.get_subcommands().map(|item| item.get_name()).collect::<Vec<_>>();

        assert_eq!(names, ["serve", "init", "server", "client", "edge", "enrollment"]);
        let command = Cli::command();
        for role in ["server", "client", "edge"] {
            let role_command = command.find_subcommand(role).unwrap();
            let actions = role_command.get_subcommands().map(|item| item.get_name()).collect::<Vec<_>>();
            assert_eq!(actions, ["issue", "renew", "revoke"]);
        }
        assert!(Cli::command().get_version().is_some());
        assert_no_plaintext_password_argument(&Cli::command());
    }

    #[test]
    fn enrollment_create_defaults_to_ten_minutes() {
        let cli = Cli::try_parse_from([
            "dbx-gateway-pki",
            "enrollment",
            "create",
            "--data-dir",
            "/tmp/pki",
            "--edge-id",
            "edge-prod-01",
        ])
        .unwrap();
        let Command::Enrollment(command) = cli.command else {
            panic!("expected enrollment command");
        };
        let EnrollmentAction::Create(args) = command.command else {
            panic!("expected enrollment create command");
        };

        assert_eq!(args.ttl, std::time::Duration::from_secs(600));
    }

    #[test]
    fn server_issue_accepts_an_ip_san_without_a_dns_san() {
        for ip in ["192.0.2.53", "2001:db8::53"] {
            let cli = Cli::try_parse_from([
                "dbx-gateway-pki",
                "server",
                "issue",
                "--data-dir",
                "/tmp/pki",
                "--password-file",
                "/tmp/password",
                "--identity",
                "main-gateway",
                "--ip-san",
                ip,
                "--output-dir",
                "/tmp/output",
            ])
            .unwrap();
            let Command::Server(command) = cli.command else {
                panic!("expected server command");
            };
            let ServerAction::Issue(args) = command.command else {
                panic!("expected server issue command");
            };

            assert!(args.dns_sans.is_empty());
            assert_eq!(args.ip_sans, [ip.parse::<std::net::IpAddr>().unwrap()]);
        }
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
