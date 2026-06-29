# NixOS / system-manager module for aws-nix-cache.
#
# Works on NixOS and on non-NixOS distros via system-manager.
# Configures the nix-daemon to read AWS credentials from a file
# written by `aws-nix-cache serve --credentials-file`.
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

  credentialsFile =
    if cfg.credentialsFile != null
    then cfg.credentialsFile
    else "/run/user/${toString config.users.users.${cfg.user}.uid}/aws-nix-cache/credentials";
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
        This user must run `aws-nix-cache serve` (manually or via a systemd user service).
      '';
    };

    profile = mkOption {
      type = types.str;
      default = "nix-cache";
      example = "nix-cache";
      description = ''
        AWS profile name. Must match the ?profile= query parameter in your
        substituter URL. For example, if your substituter is
        s3://bucket?profile=nix-cache, set this to "nix-cache".
      '';
    };

    credentialsFile = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = ''
        Path to the credentials file written by aws-nix-cache serve.
        When null (default), uses /etc/nix/aws-nix-cache-credentials.
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
    # Tell the nix-daemon to read credentials from the file written by
    # aws-nix-cache serve --credentials-file.
    systemd.services.nix-daemon.environment = {
      AWS_SHARED_CREDENTIALS_FILE = credentialsFile;
      AWS_PROFILE = cfg.profile;
    };

    # Add the S3 substituters and their signing keys to nix.conf
    nix.settings = mkIf (cfg.substituters != [ ]) {
      extra-substituters = cfg.substituters;
      extra-trusted-public-keys = cfg.trustedPublicKeys;
    };
  };
}
