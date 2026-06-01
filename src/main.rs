use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use aws_config::BehaviorVersion;
use aws_credential_types::provider::ProvideCredentials;
use clap::{Parser, Subcommand};
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

#[derive(Parser)]
#[command(
    name = "aws-nix-cache",
    about = "Bridge user AWS credentials to the Nix daemon for S3 binary cache substituters"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the credential proxy on a Unix socket.
    ///
    /// Listens for connections, verifies the peer UID via SO_PEERCRED (only
    /// root and the socket owner are allowed), then returns the current AWS
    /// credentials in credential_process JSON format.
    ///
    /// Security: access is enforced at the kernel level — no tokens, no HTTP.
    /// The socket is created with mode 0660 inside a 0700 directory under
    /// $XDG_RUNTIME_DIR, so only the owner and root (nix-daemon) can connect.
    Serve {
        /// Path to the Unix socket.
        /// Defaults to $XDG_RUNTIME_DIR/aws-nix-cache/credentials.sock
        #[arg(long, env = "AWS_NIX_CACHE_SOCKET")]
        socket: Option<PathBuf>,
    },

    /// Fetch credentials from the proxy (used as AWS credential_process).
    ///
    /// Connects to the Unix socket, reads credential_process JSON, and prints
    /// it to stdout. Configure this in the Nix daemon's AWS config:
    ///
    ///   [default]
    ///   credential_process = aws-nix-cache fetch
    Fetch {
        /// Path to the Unix socket.
        #[arg(long, env = "AWS_NIX_CACHE_SOCKET")]
        socket: Option<PathBuf>,
    },

    /// Validate current AWS credentials and print caller identity.
    Check,

    /// Print configuration for the Nix daemon.
    PrintEnv {
        /// Path to the Unix socket.
        #[arg(long, env = "AWS_NIX_CACHE_SOCKET")]
        socket: Option<PathBuf>,
    },
}

/// AWS credential_process JSON format (Version 1).
/// The AWS SDK invokes `credential_process`, reads stdout, and parses this.
#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct CredentialProcessResponse {
    version: u32,
    access_key_id: String,
    secret_access_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expiration: Option<String>,
}

// ── Paths & permissions ──────────────────────────────────────────────────

fn default_socket_path() -> PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        // /run/user/$UID — tmpfs, per-user, mode 0700 on systemd systems
        PathBuf::from(runtime_dir)
            .join("aws-nix-cache")
            .join("credentials.sock")
    } else {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        PathBuf::from(home)
            .join(".aws-nix-cache")
            .join("credentials.sock")
    }
}

fn ensure_socket_dir(socket_path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
        // Owner-only traversal (0700) — other users can't even list the directory
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    // Remove stale socket from a previous run
    if socket_path.exists() {
        std::fs::remove_file(socket_path)?;
    }
    Ok(())
}

fn current_uid() -> u32 {
    // Safety: getuid() is always safe — no failure modes, no pointers
    unsafe { libc::getuid() }
}

// ── Formatting ───────────────────────────────────────────────────────────

fn format_expiry(t: SystemTime) -> String {
    let secs = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    chrono::DateTime::from_timestamp(secs as i64, 0)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_default()
}

// ── Server ───────────────────────────────────────────────────────────────

async fn handle_connection(
    mut stream: UnixStream,
    sdk_config: &aws_config::SdkConfig,
) -> anyhow::Result<()> {
    let provider = sdk_config
        .credentials_provider()
        .ok_or_else(|| anyhow::anyhow!("no credentials provider configured"))?;

    let creds = provider
        .provide_credentials()
        .await
        .map_err(|e| anyhow::anyhow!("credential error: {e}"))?;

    let response = CredentialProcessResponse {
        version: 1,
        access_key_id: creds.access_key_id().to_string(),
        secret_access_key: creds.secret_access_key().to_string(),
        session_token: creds.session_token().map(|s| s.to_string()),
        expiration: creds.expiry().map(format_expiry),
    };

    let json = serde_json::to_string(&response)?;
    stream.write_all(json.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

async fn accept_loop(
    listener: UnixListener,
    sdk_config: Arc<aws_config::SdkConfig>,
    my_uid: u32,
) -> anyhow::Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;

        // ── SO_PEERCRED: kernel-enforced UID check ──────────────────
        // Only allow:
        //   UID 0     — root (nix-daemon)
        //   our UID   — the user who started the proxy
        // Everything else is rejected before any data is exchanged.
        let cred = stream.peer_cred()?;
        let peer_uid = cred.uid();
        if peer_uid != 0 && peer_uid != my_uid {
            tracing::warn!(
                peer_uid,
                peer_pid = ?cred.pid(),
                "rejected connection from unauthorized UID"
            );
            continue;
        }

        tracing::debug!(peer_uid, peer_pid = ?cred.pid(), "accepted connection");

        let sdk_config = Arc::clone(&sdk_config);
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, &sdk_config).await {
                tracing::error!("connection error: {e}");
            }
        });
    }
}

async fn run_serve(socket_path: PathBuf) -> anyhow::Result<()> {
    ensure_socket_dir(&socket_path)?;

    let listener = UnixListener::bind(&socket_path)?;

    // Socket: owner + group readable (0660).
    // Combined with parent dir 0700, only owner + root can connect.
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o660))?;

    let my_uid = current_uid();

    let sdk_config = Arc::new(aws_config::defaults(BehaviorVersion::latest()).load().await);

    // Best-effort credential validation on startup
    if let Some(provider) = sdk_config.credentials_provider() {
        match provider.provide_credentials().await {
            Ok(creds) => {
                tracing::info!(
                    access_key_id = creds.access_key_id(),
                    "credentials loaded successfully"
                );
            }
            Err(e) => {
                tracing::warn!("could not load credentials at startup: {e}");
                tracing::warn!("the proxy will retry on each connection");
            }
        }
    } else {
        tracing::warn!(
            "no credentials provider found — connections will fail until AWS is configured"
        );
    }

    tracing::info!(
        socket = %socket_path.display(),
        uid = my_uid,
        "listening (accepting UID 0 and UID {my_uid})"
    );
    tracing::info!("configure nix daemon with: aws-nix-cache print-env");

    // Graceful shutdown: clean up socket on Ctrl-C / SIGTERM
    let socket_path_cleanup = socket_path.clone();
    tokio::select! {
        result = accept_loop(listener, sdk_config, my_uid) => result,
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("shutting down");
            let _ = std::fs::remove_file(&socket_path_cleanup);
            Ok(())
        }
    }
}

// ── Client (credential_process) ──────────────────────────────────────────

async fn run_fetch(socket_path: PathBuf) -> anyhow::Result<()> {
    let mut stream = UnixStream::connect(&socket_path).await.map_err(|e| {
        anyhow::anyhow!(
            "cannot connect to {}: {e}\nIs `aws-nix-cache serve` running?",
            socket_path.display()
        )
    })?;

    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;

    // Validate JSON before printing (fail fast on corrupt data)
    let _: serde_json::Value = serde_json::from_slice(&buf)
        .map_err(|e| anyhow::anyhow!("invalid response from server: {e}"))?;

    // Print to stdout — the AWS SDK reads this as credential_process output
    use std::io::Write;
    std::io::stdout().write_all(&buf)?;
    println!();
    Ok(())
}

// ── Check ────────────────────────────────────────────────────────────────

async fn run_check() -> anyhow::Result<()> {
    let sdk_config = aws_config::defaults(BehaviorVersion::latest()).load().await;
    let sts = aws_sdk_sts::Client::new(&sdk_config);

    match sts.get_caller_identity().send().await {
        Ok(identity) => {
            println!("AWS credentials valid:");
            println!("  Account: {}", identity.account().unwrap_or("unknown"));
            println!("  Arn:     {}", identity.arn().unwrap_or("unknown"));
            println!("  UserId:  {}", identity.user_id().unwrap_or("unknown"));

            if let Some(provider) = sdk_config.credentials_provider() {
                if let Ok(creds) = provider.provide_credentials().await {
                    if let Some(expiry) = creds.expiry() {
                        println!("  Expires: {}", format_expiry(expiry));
                    }
                }
            }
            Ok(())
        }
        Err(e) => {
            anyhow::bail!("failed to validate AWS credentials: {e}");
        }
    }
}

// ── Print config ─────────────────────────────────────────────────────────

fn run_print_env(socket_path: PathBuf) {
    let binary = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "aws-nix-cache".to_string());

    let sock = socket_path.display();

    println!("# ── NixOS module (configuration.nix) ───────────────────────────");
    println!("# nix.settings.substituters = [ \"s3://YOUR-BUCKET?region=REGION\" ];");
    println!("#");
    println!("# systemd.services.nix-daemon.environment.AWS_CONFIG_FILE =");
    println!("#   \"/root/.aws/config\";");
    println!("#");
    println!("# Then write /root/.aws/config (readable by root only):");
    println!();
    println!("# ── /root/.aws/config ──────────────────────────────────────────");
    println!("[default]");
    println!("credential_process = {binary} fetch --socket {sock}");
    println!();
    println!("# ── systemd user service (~/.config/systemd/user/) ─────────────");
    println!("# [Unit]");
    println!("# Description=AWS credential proxy for Nix daemon");
    println!("# After=network.target");
    println!("#");
    println!("# [Service]");
    println!("# ExecStart={binary} serve --socket {sock}");
    println!("# Restart=always");
    println!("# RestartSec=5");
    println!("#");
    println!("# [Install]");
    println!("# WantedBy=default.target");
}

// ── Main ─────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "aws_nix_cache=info".into()),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Serve { socket } => run_serve(socket.unwrap_or_else(default_socket_path)).await,
        Command::Fetch { socket } => run_fetch(socket.unwrap_or_else(default_socket_path)).await,
        Command::Check => run_check().await,
        Command::PrintEnv { socket } => {
            run_print_env(socket.unwrap_or_else(default_socket_path));
            Ok(())
        }
    }
}
