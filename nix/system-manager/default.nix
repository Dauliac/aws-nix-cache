# System-manager module for aws-nix-cache.
#
# For non-NixOS distros using numtide/system-manager.
# Configures the nix-daemon to read AWS credentials from a file
# written by `aws-nix-cache serve --credentials-file`.
#
# Usage:
#   services.aws-nix-cache = {
#     enable = true;
#     uid = 1001;             # UID of the user running `aws-nix-cache serve`
#     profile = "nix-cache";  # must match ?profile= in substituter URL
#   };

{ config, lib, pkgs, ... }:
let
  inherit (lib) mkOption mkEnableOption mkIf types;
  cfg = config.services.aws-nix-cache;

  credentialsFile =
    if cfg.credentialsFile != null
    then cfg.credentialsFile
    else "/etc/nix/aws-nix-cache-credentials";
in
{
  options.services.aws-nix-cache = {
    enable = mkEnableOption "AWS credential proxy for Nix S3 binary caches";

    uid = mkOption {
      type = types.int;
      description = ''
        UID of the user running `aws-nix-cache serve`.
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

    region = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "eu-west-3";
      description = "AWS region for the nix-daemon.";
    };
  };

  config = mkIf cfg.enable {
    # Tell the nix-daemon to read credentials from the file written by
    # aws-nix-cache serve --credentials-file.
    # The C++ AWS SDK reads AWS_SHARED_CREDENTIALS_FILE + AWS_PROFILE directly.
    environment.etc."systemd/system/nix-daemon.service.d/50-aws-nix-cache.conf" = {
      text = ''
        [Service]
        Environment="AWS_SHARED_CREDENTIALS_FILE=${credentialsFile}"
        Environment="AWS_PROFILE=${cfg.profile}"
      '' + lib.optionalString (cfg.region != null) ''
        Environment="AWS_DEFAULT_REGION=${cfg.region}"
      '';
    };
  };
}
