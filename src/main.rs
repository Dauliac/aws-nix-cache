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

        /// AWS profile to read credentials from (your user profile, not the
        /// daemon profile). Maps to AWS_PROFILE. Required if your default
        /// profile has no credentials (e.g. SSO with AWS_DEFAULT_PROFILE).
        #[arg(long, env = "AWS_PROFILE")]
        aws_profile: Option<String>,
    },

    /// Fetch credentials from the proxy (used as AWS credential_process).
    ///
    /// Connects to the Unix socket, reads credential_process JSON, and prints
    /// it to stdout. The nix-daemon invokes this via credential_process in
    /// /root/.aws/config.
    Fetch {
        /// Path to the Unix socket.
        #[arg(long, env = "AWS_NIX_CACHE_SOCKET")]
        socket: Option<PathBuf>,
    },

    /// Write /root/.aws/config so the nix-daemon can use credential_process.
    ///
    /// The profile name must match the `?profile=` parameter in your
    /// substituter URL (e.g. s3://bucket?profile=nix-cache → --profile nix-cache).
    /// If your substituter has no `?profile=`, use --profile default.
    ///
    /// This writes (or updates) the AWS config for root. Requires root or sudo.
    /// Works on any distro (NixOS, Ubuntu, Fedora, etc.).
    Setup {
        /// AWS profile name — must match the ?profile= in your substituter URL.
        #[arg(long, default_value = "default")]
        profile: String,

        /// Path to the Unix socket.
        #[arg(long, env = "AWS_NIX_CACHE_SOCKET")]
        socket: Option<PathBuf>,

        /// AWS config file to write. Defaults to /root/.aws/config.
        #[arg(long, default_value = "/root/.aws/config")]
        config_file: PathBuf,

        /// Print the config to stdout instead of writing the file.
        #[arg(long)]
        dry_run: bool,
    },

    /// Validate current AWS credentials and print caller identity.
    Check {
        /// AWS profile to use for credential resolution.
        #[arg(long, env = "AWS_PROFILE")]
        aws_profile: Option<String>,
    },

    /// Install a systemd user service for `aws-nix-cache serve`.
    ///
    /// Works on any distro with systemd (Ubuntu, Fedora, NixOS, etc.).
    /// Writes ~/.config/systemd/user/aws-nix-cache.service, then runs
    /// daemon-reload and enables the service.
    InstallService {
        /// Path to the Unix socket.
        #[arg(long, env = "AWS_NIX_CACHE_SOCKET")]
        socket: Option<PathBuf>,

        /// AWS profile to read credentials from.
        #[arg(long)]
        aws_profile: Option<String>,

        /// Print the unit file to stdout instead of installing.
        #[arg(long)]
        dry_run: bool,
    },

    /// Print full setup instructions for any distro.
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

// ── AWS config ───────────────────────────────────────────────────────────

async fn load_sdk_config(aws_profile: Option<&str>) -> aws_config::SdkConfig {
    let mut loader = aws_config::defaults(BehaviorVersion::latest());
    if let Some(profile) = aws_profile {
        loader = loader.profile_name(profile);
    }
    loader.load().await
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

async fn run_serve(socket_path: PathBuf, aws_profile: Option<String>) -> anyhow::Result<()> {
    ensure_socket_dir(&socket_path)?;

    let listener = UnixListener::bind(&socket_path)?;

    // Socket: owner + group readable (0660).
    // Combined with parent dir 0700, only owner + root can connect.
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o660))?;

    let my_uid = current_uid();

    let sdk_config = Arc::new(load_sdk_config(aws_profile.as_deref()).await);

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

async fn run_check(aws_profile: Option<String>) -> anyhow::Result<()> {
    let sdk_config = load_sdk_config(aws_profile.as_deref()).await;
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

// ── Setup (write /root/.aws/config) ──────────────────────────────────────

fn credential_process_line(socket_path: &Path) -> String {
    let binary = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "aws-nix-cache".to_string());
    let sock = socket_path.display();
    format!("credential_process = {binary} fetch --socket {sock}")
}

fn generate_aws_config(profile: &str, socket_path: &Path) -> String {
    let cred_line = credential_process_line(socket_path);
    if profile == "default" {
        format!("[default]\n{cred_line}\n")
    } else {
        format!("[profile {profile}]\n{cred_line}\n")
    }
}

fn run_setup(
    profile: String,
    socket_path: PathBuf,
    config_file: PathBuf,
    dry_run: bool,
) -> anyhow::Result<()> {
    let config_content = generate_aws_config(&profile, &socket_path);

    if dry_run {
        println!("{config_content}");
        return Ok(());
    }

    // Check we're root (or tell the user to use sudo)
    if current_uid() != 0 {
        eprintln!(
            "error: setup must run as root to write {}",
            config_file.display()
        );
        eprintln!();
        eprintln!("  sudo aws-nix-cache setup --profile {profile}");
        eprintln!();
        eprintln!("Or preview with --dry-run:");
        eprintln!("  aws-nix-cache setup --profile {profile} --dry-run");
        std::process::exit(1);
    }

    // Create parent directory only if it doesn't exist.
    // Never change permissions on existing directories (e.g. /etc/nix/).
    if let Some(parent) = config_file.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent)?;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        }
    }

    // If the file exists, try to update just our profile section
    if config_file.exists() {
        let existing = std::fs::read_to_string(&config_file)?;
        let updated = update_profile_in_config(&existing, &profile, &socket_path);
        std::fs::write(&config_file, &updated)?;
        eprintln!("updated profile [{profile}] in {}", config_file.display());
    } else {
        std::fs::write(&config_file, &config_content)?;
        eprintln!("created {}", config_file.display());
    }

    std::fs::set_permissions(&config_file, std::fs::Permissions::from_mode(0o600))?;

    eprintln!();
    eprintln!("next steps:");
    eprintln!("  1. run `aws-nix-cache serve` as your user");
    eprintln!("  2. restart nix-daemon: sudo systemctl restart nix-daemon");
    Ok(())
}

/// Update or append a profile section in an existing AWS config file.
/// Preserves all other content.
fn update_profile_in_config(existing: &str, profile: &str, socket_path: &Path) -> String {
    let section_header = if profile == "default" {
        "[default]".to_string()
    } else {
        format!("[profile {profile}]")
    };

    let cred_line = credential_process_line(socket_path);
    let mut result = String::new();
    let mut in_our_section = false;
    let mut wrote_our_section = false;

    for line in existing.lines() {
        let trimmed = line.trim();

        if trimmed == section_header {
            // Start of our section — write the replacement
            result.push_str(&section_header);
            result.push('\n');
            result.push_str(&cred_line);
            result.push('\n');
            in_our_section = true;
            wrote_our_section = true;
            continue;
        }

        // Detect start of a different section
        if trimmed.starts_with('[') && in_our_section {
            in_our_section = false;
        }

        // Skip lines from our old section
        if in_our_section {
            continue;
        }

        result.push_str(line);
        result.push('\n');
    }

    // If the profile wasn't found, append it
    if !wrote_our_section {
        if !result.ends_with('\n') && !result.is_empty() {
            result.push('\n');
        }
        result.push('\n');
        result.push_str(&section_header);
        result.push('\n');
        result.push_str(&cred_line);
        result.push('\n');
    }

    result
}

// ── Install systemd user service ─────────────────────────────────────────

fn generate_unit_file(socket_path: &Path, aws_profile: Option<&str>) -> String {
    let binary = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "aws-nix-cache".to_string());
    let sock = socket_path.display();

    let mut exec = format!("{binary} serve --socket {sock}");
    if let Some(profile) = aws_profile {
        exec.push_str(&format!(" --aws-profile {profile}"));
    }

    format!(
        "\
[Unit]
Description=AWS credential proxy for Nix daemon
Documentation=https://github.com/Dauliac/aws-nix-cache

[Service]
ExecStart={exec}
Restart=always
RestartSec=5

[Install]
WantedBy=default.target
"
    )
}

fn run_install_service(
    socket_path: PathBuf,
    aws_profile: Option<String>,
    dry_run: bool,
) -> anyhow::Result<()> {
    let unit = generate_unit_file(&socket_path, aws_profile.as_deref());

    if dry_run {
        println!("{unit}");
        return Ok(());
    }

    let home = std::env::var("HOME")
        .map_err(|_| anyhow::anyhow!("HOME not set — cannot determine user systemd path"))?;
    let unit_dir = PathBuf::from(&home).join(".config/systemd/user");
    let unit_path = unit_dir.join("aws-nix-cache.service");

    std::fs::create_dir_all(&unit_dir)?;
    std::fs::write(&unit_path, &unit)?;
    eprintln!("wrote {}", unit_path.display());

    // daemon-reload + enable + start
    let reload = std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    if let Err(e) = reload {
        eprintln!("warning: systemctl daemon-reload failed: {e}");
        eprintln!("run manually: systemctl --user daemon-reload");
    }

    let enable = std::process::Command::new("systemctl")
        .args(["--user", "enable", "--now", "aws-nix-cache.service"])
        .status();
    match enable {
        Ok(s) if s.success() => {
            eprintln!("service enabled and started");
        }
        Ok(s) => {
            eprintln!("warning: systemctl enable --now exited with {s}");
            eprintln!("run manually: systemctl --user enable --now aws-nix-cache.service");
        }
        Err(e) => {
            eprintln!("warning: could not run systemctl: {e}");
            eprintln!("run manually:");
            eprintln!("  systemctl --user daemon-reload");
            eprintln!("  systemctl --user enable --now aws-nix-cache.service");
        }
    }

    Ok(())
}

// ── Print config ─────────────────────────────────────────────────────────

fn run_print_env(socket_path: PathBuf) {
    let binary = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "aws-nix-cache".to_string());

    let sock = socket_path.display();

    println!("# ── Quick setup (any distro: Ubuntu, Fedora, NixOS, ...) ───────");
    println!("#");
    println!("# 1. Configure root's AWS config (matches ?profile= in substituter URL):");
    println!("#    sudo aws-nix-cache setup --profile nix-cache");
    println!("#");
    println!("# 2. Start the proxy as your user:");
    println!("#    aws-nix-cache serve");
    println!("#");
    println!("# 3. Restart the daemon:");
    println!("#    sudo systemctl restart nix-daemon");
    println!();
    println!("# ── /root/.aws/config (generated by `setup`) ───────────────────");
    println!("[profile nix-cache]");
    println!("credential_process = {binary} fetch --socket {sock}");
    println!();
    println!("# ── /etc/nix/nix.conf or /etc/nix/conf.d/substituters.conf ─────");
    println!("# extra-substituters = s3://YOUR-BUCKET?region=REGION&profile=nix-cache");
    println!("# extra-trusted-public-keys = YOUR-KEY");
    println!();
    println!("# ── NixOS / system-manager module ──────────────────────────────");
    println!("# imports = [ inputs.aws-nix-cache.nixosModules.default ];");
    println!("#");
    println!("# services.aws-nix-cache = {{");
    println!("#   enable = true;");
    println!("#   package = inputs.aws-nix-cache.packages.${{system}}.default;");
    println!("#   user = \"your-username\";");
    println!("#   profile = \"nix-cache\";");
    println!("#   substituters = [ \"s3://bucket?region=eu-west-3&profile=nix-cache\" ];");
    println!("#   trustedPublicKeys = [ \"cache:AAAA...=\" ];");
    println!("# }};");
    println!();
    println!("# ── systemd user service (optional, for auto-start) ────────────");
    println!("# ~/.config/systemd/user/aws-nix-cache.service");
    println!("# [Unit]");
    println!("# Description=AWS credential proxy for Nix daemon");
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
        Command::Serve {
            socket,
            aws_profile,
        } => run_serve(socket.unwrap_or_else(default_socket_path), aws_profile).await,
        Command::Fetch { socket } => run_fetch(socket.unwrap_or_else(default_socket_path)).await,
        Command::Setup {
            profile,
            socket,
            config_file,
            dry_run,
        } => run_setup(
            profile,
            socket.unwrap_or_else(default_socket_path),
            config_file,
            dry_run,
        ),
        Command::InstallService {
            socket,
            aws_profile,
            dry_run,
        } => run_install_service(
            socket.unwrap_or_else(default_socket_path),
            aws_profile,
            dry_run,
        ),
        Command::Check { aws_profile } => run_check(aws_profile).await,
        Command::PrintEnv { socket } => {
            run_print_env(socket.unwrap_or_else(default_socket_path));
            Ok(())
        }
    }
}
