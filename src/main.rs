use std::io::Read;
use std::net::SocketAddr;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use aws_config::BehaviorVersion;
use aws_credential_types::provider::ProvideCredentials;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use clap::{Parser, Subcommand};
use serde::Serialize;

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
    /// Start the credential proxy server.
    ///
    /// Runs an HTTP server on localhost that serves your current AWS credentials
    /// in the ECS container credential format. Configure the Nix daemon to use
    /// AWS_CONTAINER_CREDENTIALS_FULL_URI pointing to this server.
    ///
    /// An authorization token is always required. If not provided via --auth-token
    /// or AWS_NIX_CACHE_AUTH_TOKEN, one is auto-generated and saved to the token file.
    /// Only processes that know the token (the Nix daemon) can fetch credentials.
    Serve {
        /// Address to bind to. MUST be a loopback address (127.0.0.1 or [::1]).
        /// Binding to non-loopback addresses is refused to prevent credential exposure.
        #[arg(long, default_value = "127.0.0.1:23456")]
        bind: SocketAddr,

        /// Authorization token for request authentication.
        /// If omitted, a secure random token is auto-generated and saved to --token-file.
        /// The Nix daemon must send this token via AWS_CONTAINER_AUTHORIZATION_TOKEN.
        #[arg(long, env = "AWS_NIX_CACHE_AUTH_TOKEN")]
        auth_token: Option<String>,

        /// Path to store/read the auth token. Defaults to $XDG_RUNTIME_DIR/aws-nix-cache/token
        /// (typically /run/user/$UID/aws-nix-cache/token). Created with mode 0600.
        #[arg(long, env = "AWS_NIX_CACHE_TOKEN_FILE")]
        token_file: Option<PathBuf>,
    },

    /// Validate current AWS credentials and print caller identity.
    Check,

    /// Print environment configuration for the Nix daemon.
    ///
    /// Reads the auth token from --auth-token or the token file.
    /// Run `aws-nix-cache serve` first to generate the token.
    PrintEnv {
        /// Address the proxy listens on
        #[arg(long, default_value = "127.0.0.1:23456")]
        bind: SocketAddr,

        /// Authorization token. If omitted, reads from the token file.
        #[arg(long, env = "AWS_NIX_CACHE_AUTH_TOKEN")]
        auth_token: Option<String>,

        /// Path to the auth token file.
        #[arg(long, env = "AWS_NIX_CACHE_TOKEN_FILE")]
        token_file: Option<PathBuf>,
    },
}

struct AppState {
    sdk_config: aws_config::SdkConfig,
    auth_token: String,
}

/// ECS-compatible credential response.
/// The AWS SDK expects exactly this JSON shape when using
/// AWS_CONTAINER_CREDENTIALS_FULL_URI.
#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct CredentialResponse {
    access_key_id: String,
    secret_access_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expiration: Option<String>,
}

// ── Security helpers ─────────────────────────────────────────────────────

fn default_token_path() -> PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        // /run/user/$UID — tmpfs, per-user, mode 0700 on systemd systems
        PathBuf::from(runtime_dir)
            .join("aws-nix-cache")
            .join("token")
    } else {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        PathBuf::from(home).join(".aws-nix-cache").join("token")
    }
}

fn generate_token() -> anyhow::Result<String> {
    let mut buf = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

fn save_token(path: &Path, token: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        // Directory: owner-only access (0700)
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    // File: owner-only read/write (0600) — root can still read (CAP_DAC_OVERRIDE)
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    std::fs::write(path, token)?;
    Ok(())
}

fn load_token(path: &Path) -> anyhow::Result<String> {
    let token = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("cannot read token file {}: {e}", path.display()))?
        .trim()
        .to_string();
    anyhow::ensure!(!token.is_empty(), "token file {} is empty", path.display());
    Ok(token)
}

/// Resolve the auth token: explicit > file > auto-generate.
fn resolve_token(
    auth_token: Option<String>,
    token_file: Option<PathBuf>,
    generate_if_missing: bool,
) -> anyhow::Result<String> {
    // 1. Explicit token takes priority
    if let Some(token) = auth_token {
        return Ok(token);
    }

    let path = token_file.unwrap_or_else(default_token_path);

    // 2. Try to read from file
    if path.exists() {
        match load_token(&path) {
            Ok(token) => {
                tracing::info!(path = %path.display(), "loaded auth token from file");
                return Ok(token);
            }
            Err(e) => {
                tracing::warn!("failed to load token from {}: {e}", path.display());
            }
        }
    }

    // 3. Auto-generate if allowed
    if generate_if_missing {
        let token = generate_token()?;
        save_token(&path, &token)?;
        tracing::info!(path = %path.display(), "generated new auth token (saved with mode 0600)");
        return Ok(token);
    }

    anyhow::bail!(
        "no auth token found. Run `aws-nix-cache serve` first to generate one, \
         or pass --auth-token / set AWS_NIX_CACHE_AUTH_TOKEN"
    )
}

fn validate_loopback(addr: &SocketAddr) -> anyhow::Result<()> {
    anyhow::ensure!(
        addr.ip().is_loopback(),
        "refusing to bind to non-loopback address {addr} — \
         this would expose AWS credentials to the network. \
         Use 127.0.0.1 or [::1]"
    );
    Ok(())
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

// ── HTTP handlers ────────────────────────────────────────────────────────

async fn health() -> &'static str {
    "ok"
}

async fn credentials(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    // Auth token is always required — checked via constant-time comparison
    let provided = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !constant_time_eq(provided.as_bytes(), state.auth_token.as_bytes()) {
        return (
            StatusCode::UNAUTHORIZED,
            "invalid or missing authorization token",
        )
            .into_response();
    }

    let provider = match state.sdk_config.credentials_provider() {
        Some(p) => p,
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "no credentials provider configured",
            )
                .into_response();
        }
    };

    match provider.provide_credentials().await {
        Ok(creds) => {
            tracing::debug!(access_key_id = creds.access_key_id(), "serving credentials");
            let response = CredentialResponse {
                access_key_id: creds.access_key_id().to_string(),
                secret_access_key: creds.secret_access_key().to_string(),
                token: creds.session_token().map(|s| s.to_string()),
                expiration: creds.expiry().map(format_expiry),
            };
            Json(response).into_response()
        }
        Err(e) => {
            tracing::error!("failed to provide credentials: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("credential error: {e}"),
            )
                .into_response()
        }
    }
}

/// Constant-time byte comparison to prevent timing attacks on the auth token.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

// ── Subcommand implementations ───────────────────────────────────────────

async fn run_serve(
    bind: SocketAddr,
    auth_token: Option<String>,
    token_file: Option<PathBuf>,
) -> anyhow::Result<()> {
    validate_loopback(&bind)?;

    let auth_token = resolve_token(auth_token, token_file, true)?;

    let sdk_config = aws_config::defaults(BehaviorVersion::latest()).load().await;

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
                tracing::warn!("the proxy will retry on each request");
            }
        }
    } else {
        tracing::warn!(
            "no credentials provider found — requests will fail until AWS is configured"
        );
    }

    let state = Arc::new(AppState {
        sdk_config,
        auth_token,
    });

    let app = Router::new()
        .route("/health", get(health))
        .route("/credentials", get(credentials))
        .with_state(state);

    tracing::info!(%bind, "starting credential proxy (auth token required)");
    tracing::info!("configure nix daemon with: aws-nix-cache print-env");

    let listener = tokio::net::TcpListener::bind(bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

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

fn run_print_env(
    bind: SocketAddr,
    auth_token: Option<String>,
    token_file: Option<PathBuf>,
) -> anyhow::Result<()> {
    let token = resolve_token(auth_token, token_file, false)?;
    let uri = format!("http://{bind}/credentials");

    println!("# ── NixOS (configuration.nix) ──────────────────────────────────");
    println!("# systemd.services.nix-daemon.environment = {{");
    println!("#   AWS_CONTAINER_CREDENTIALS_FULL_URI = \"{uri}\";");
    println!("#   AWS_CONTAINER_AUTHORIZATION_TOKEN = \"{token}\";");
    println!("# }};");
    println!();
    println!("# ── systemd override (systemctl edit nix-daemon) ───────────────");
    println!("# [Service]");
    println!("# Environment=\"AWS_CONTAINER_CREDENTIALS_FULL_URI={uri}\"");
    println!("# Environment=\"AWS_CONTAINER_AUTHORIZATION_TOKEN={token}\"");
    println!();
    println!("# ── Shell export ────────────────────────────────────────────────");
    println!("export AWS_CONTAINER_CREDENTIALS_FULL_URI=\"{uri}\"");
    println!("export AWS_CONTAINER_AUTHORIZATION_TOKEN=\"{token}\"");
    Ok(())
}

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
            bind,
            auth_token,
            token_file,
        } => run_serve(bind, auth_token, token_file).await,
        Command::Check => run_check().await,
        Command::PrintEnv {
            bind,
            auth_token,
            token_file,
        } => run_print_env(bind, auth_token, token_file),
    }
}
