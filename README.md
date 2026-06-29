# aws-nix-cache

Bridge your user AWS credentials to the Nix daemon for S3 binary cache substituters.

## Problem

The Nix daemon runs as root and has no access to your user's AWS credentials (SSO, IAM Identity Center, etc.). This means S3 binary cache substituters fail silently when the daemon can't authenticate.

The Lix/Nix C++ AWS SDK does **not** support `credential_process` — it can only read credentials from files or environment variables.

## Solution

`aws-nix-cache` runs as a systemd user service that:

1. Reads your AWS credentials (SSO, env vars, profiles — anything the AWS SDK supports)
2. Writes them to a shared credentials file that root can read
3. Refreshes the file periodically (default: every 5 minutes)
4. Optionally exposes a Unix socket for direct credential fetching

Security is kernel-enforced: the socket uses `SO_PEERCRED` to allow only UID 0 (nix-daemon) and the socket owner. The socket directory is mode `0700`, the socket is mode `0660`, and the credentials file is mode `0600`.

## Quick Start

### Install with Nix Flakes

Add to your flake inputs:

```nix
{
  inputs.aws-nix-cache.url = "github:Dauliac/aws-nix-cache";
}
```

### Any Distro (Manual Setup)

```bash
# 1. Build and install
nix profile install github:Dauliac/aws-nix-cache

# 2. Start the credential proxy (as your user)
aws-nix-cache serve --aws-profile your-profile --credentials-file /run/user/$(id -u)/aws-nix-cache/credentials

# 3. Install as a systemd user service (optional, for auto-start)
aws-nix-cache install-service --aws-profile your-profile

# 4. Configure the nix-daemon to use the credentials file
# Add to /etc/nix/nix.conf:
#   extra-substituters = s3://your-bucket?region=eu-west-3&profile=nix-cache
#   extra-trusted-public-keys = your-cache:AAAA...=
#
# Set daemon environment (e.g. via systemd drop-in):
#   AWS_SHARED_CREDENTIALS_FILE=/run/user/<UID>/aws-nix-cache/credentials
#   AWS_PROFILE=nix-cache

# 5. Restart the daemon
sudo systemctl restart nix-daemon

# 6. Verify
aws-nix-cache check --aws-profile your-profile
```

### NixOS Module

```nix
{ inputs, ... }:
{
  imports = [ inputs.aws-nix-cache.nixosModules.default ];

  services.aws-nix-cache = {
    enable = true;
    package = inputs.aws-nix-cache.packages.${system}.default;
    user = "your-username";
    profile = "nix-cache";
    substituters = [ "s3://your-bucket?region=eu-west-3&profile=nix-cache" ];
    trustedPublicKeys = [ "your-cache:AAAA...=" ];
  };
}
```

This configures the nix-daemon's environment (`AWS_SHARED_CREDENTIALS_FILE`, `AWS_PROFILE`) and adds the substituters to `nix.settings`.

### Home Manager Module

```nix
{ inputs, ... }:
{
  imports = [ inputs.aws-nix-cache.homeManagerModules.default ];

  services.aws-nix-cache = {
    enable = true;
    package = inputs.aws-nix-cache.packages.${pkgs.system}.default;
    awsProfile = "your-profile";
    # credentialsFile defaults to $XDG_RUNTIME_DIR/aws-nix-cache/credentials
    # credentialsProfile defaults to "nix-cache"
  };
}
```

This installs a systemd user service and sets `AWS_SHARED_CREDENTIALS_FILE` in your session variables.

### System Manager Module (Non-NixOS)

For non-NixOS distros using [numtide/system-manager](https://github.com/numtide/system-manager):

```nix
{ inputs, ... }:
{
  imports = [ inputs.aws-nix-cache.systemManagerModules.default ];

  services.aws-nix-cache = {
    enable = true;
    uid = 1001; # UID of the user running `aws-nix-cache serve`
    profile = "nix-cache";
  };
}
```

This creates a systemd drop-in for `nix-daemon.service` that sets the required environment variables.

## CLI Reference

```
aws-nix-cache serve    Start the credential proxy and file writer
  --socket <PATH>                Unix socket path (default: $XDG_RUNTIME_DIR/aws-nix-cache/credentials.sock)
  --aws-profile <PROFILE>       AWS profile to read credentials from
  --credentials-file <PATH>     Write credentials to this file for the daemon
  --credentials-profile <NAME>  Profile name in the credentials file (default: nix-cache)
  --credentials-refresh-secs <N> Refresh interval in seconds (default: 300)

aws-nix-cache fetch    Fetch credentials from the proxy (credential_process client)
  --socket <PATH>

aws-nix-cache check    Validate AWS credentials via STS GetCallerIdentity
  --aws-profile <PROFILE>

aws-nix-cache setup    Write /root/.aws/config for credential_process (requires root)
  --profile <NAME>     Profile name (default: default)
  --config-file <PATH> AWS config file (default: /root/.aws/config)
  --dry-run            Print config instead of writing

aws-nix-cache install-service  Install a systemd user service
  --aws-profile <PROFILE>
  --dry-run

aws-nix-cache print-env  Print full setup instructions
```

## How It Works

```
 User session                          Root / nix-daemon
 ─────────────                         ─────────────────
 aws-nix-cache serve                   nix-daemon
   │                                     │
   ├─ reads AWS creds (SSO, env, etc.)   │
   │                                     │
   ├─ writes credentials file ──────────►│ reads via AWS_SHARED_CREDENTIALS_FILE
   │  (atomically, every 5min)           │
   │                                     │
   └─ listens on Unix socket             │
      (SO_PEERCRED: UID 0 + owner)       └─ uses creds for S3 substituter
```

## Development

```bash
nix develop   # enter devshell with cargo
cargo build   # build
cargo run -- serve --aws-profile your-profile  # run locally
```

## License

MIT
