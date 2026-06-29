# System-manager module for aws-nix-cache.
#
# For non-NixOS distros using numtide/system-manager.
# Configures the nix-daemon to fetch AWS credentials from the
# aws-nix-cache Unix socket proxy via credential_process.
#
# Usage:
#   services.aws-nix-cache = {
#     enable = true;
#     package = inputs.aws-nix-cache.packages.${system}.default;
#     uid = 1001;             # UID of the user running `aws-nix-cache serve`
#     profile = "nix-cache";  # must match ?profile= in substituter URL
#   };

{ config, lib, pkgs, ... }:
let
  inherit (lib) mkOption mkEnableOption mkIf types;
  cfg = config.services.aws-nix-cache;

  socketPath =
    if cfg.socketPath != null
    then cfg.socketPath
    else "/run/user/${toString cfg.uid}/aws-nix-cache/credentials.sock";

  profileHeader =
    if cfg.profile == "default"
    then "[default]"
    else "[profile ${cfg.profile}]";

  awsConfigContent = ''
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

    uid = mkOption {
      type = types.int;
      description = ''
        UID of the user running `aws-nix-cache serve`.
        Used to derive the socket path under /run/user/<UID>/.
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
        the uid option: /run/user/<UID>/aws-nix-cache/credentials.sock.
      '';
    };

    region = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "eu-west-3";
      description = "AWS region to include in the config profile.";
    };
  };

  config = mkIf cfg.enable {
    environment.etc."nix/aws-nix-cache-config" = {
      text = awsConfigContent + lib.optionalString (cfg.region != null) ''
        region = ${cfg.region}
      '';
    };

    environment.etc."systemd/system/nix-daemon.service.d/50-aws-nix-cache.conf" = {
      text = ''
        [Service]
        Environment="AWS_CONFIG_FILE=/etc/nix/aws-nix-cache-config"
        Environment="AWS_PROFILE=${cfg.profile}"
      '';
    };
  };
}
