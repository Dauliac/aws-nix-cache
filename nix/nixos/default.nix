# NixOS / system-manager module for aws-nix-cache.
#
# Works on NixOS and on non-NixOS distros via system-manager.
# Configures the nix-daemon to fetch AWS credentials from the
# aws-nix-cache Unix socket proxy via credential_process.
#
# Usage:
#   services.aws-nix-cache = {
#     enable = true;
#     package = inputs.aws-nix-cache.packages.${system}.default;
#     user = "myuser";
#     profile = "nix-cache";  # must match ?profile= in substituter URL
#     substituters = [ "s3://my-cache?region=eu-west-3&profile=nix-cache" ];
#     trustedPublicKeys = [ "my-cache:AAAA...=" ];
#   };

{ config, lib, pkgs, ... }:
let
  inherit (lib) mkOption mkEnableOption mkIf types;
  cfg = config.services.aws-nix-cache;

  socketPath =
    if cfg.socketPath != null
    then cfg.socketPath
    else "/run/user/${toString config.users.users.${cfg.user}.uid}/aws-nix-cache/credentials.sock";

  profileHeader =
    if cfg.profile == "default"
    then "[default]"
    else "[profile ${cfg.profile}]";

  # Written to the nix store — root reads it via AWS_CONFIG_FILE.
  awsConfigFile = pkgs.writeText "aws-nix-cache-config" ''
    ${profileHeader}
    credential_process = ${cfg.package}/bin/aws-nix-cache fetch --socket ${socketPath}
  '';
in
{
  options.services.aws-nix-cache = {
    enable = mkEnableOption "AWS credential proxy for Nix S3 binary caches";

    package = mkOption {
      type = types.package;
      description = "The aws-nix-cache package.";
    };

    user = mkOption {
      type = types.str;
      description = ''
        User account whose AWS credentials to proxy.
        The socket path defaults to /run/user/<UID>/aws-nix-cache/credentials.sock.
        This user must run `aws-nix-cache serve` (manually or via a systemd user service).
      '';
    };

    profile = mkOption {
      type = types.str;
      default = "default";
      example = "nix-cache";
      description = ''
        AWS profile name. Must match the ?profile= query parameter in your
        substituter URL. For example, if your substituter is
        s3://bucket?profile=nix-cache, set this to "nix-cache".
      '';
    };

    socketPath = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = ''
        Override the Unix socket path. When null (default), derived from
        the user option: /run/user/<UID>/aws-nix-cache/credentials.sock.
      '';
    };

    substituters = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "s3://my-nix-cache?region=eu-west-3&profile=nix-cache" ];
      description = "S3 binary cache URLs to add as Nix substituters.";
    };

    trustedPublicKeys = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "my-cache:AAAA...=" ];
      description = "Public keys to trust for the configured substituters.";
    };
  };

  config = mkIf cfg.enable {
    # Tell the nix-daemon where to find the AWS config with credential_process.
    # The daemon reads this file as root, resolves the profile, and runs
    # `aws-nix-cache fetch` which connects to the user's Unix socket.
    systemd.services.nix-daemon.environment.AWS_CONFIG_FILE =
      toString awsConfigFile;

    # Add the S3 substituters and their signing keys to nix.conf
    nix.settings = mkIf (cfg.substituters != [ ]) {
      extra-substituters = cfg.substituters;
      extra-trusted-public-keys = cfg.trustedPublicKeys;
    };
  };
}
