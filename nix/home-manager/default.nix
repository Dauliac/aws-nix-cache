# Home-manager module for aws-nix-cache.
#
# Installs the systemd user service that proxies your AWS credentials
# to the Nix daemon via a Unix socket.
#
# Usage in home-manager config:
#   imports = [ inputs.aws-nix-cache.homeManagerModules.default ];
#
#   services.aws-nix-cache = {
#     enable = true;
#     package = inputs.aws-nix-cache.packages.${pkgs.system}.default;
#     awsProfile = "manomano-support";
#   };

{ config, lib, pkgs, ... }:
let
  inherit (lib) mkOption mkEnableOption mkIf types;
  cfg = config.services.aws-nix-cache;

  socketPath =
    if cfg.socketPath != null
    then cfg.socketPath
    else "%t/aws-nix-cache/credentials.sock";
in
{
  options.services.aws-nix-cache = {
    enable = mkEnableOption "AWS credential proxy for Nix S3 binary caches";

    package = mkOption {
      type = types.package;
      description = "The aws-nix-cache package.";
    };

    awsProfile = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "manomano-support";
      description = ''
        AWS profile to read credentials from (your user profile).
        Maps to --aws-profile / AWS_PROFILE.
      '';
    };

    socketPath = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = ''
        Override the Unix socket path. When null (default), uses
        $XDG_RUNTIME_DIR/aws-nix-cache/credentials.sock (%t in systemd).
      '';
    };
  };

  config = mkIf cfg.enable {
    systemd.user.services.aws-nix-cache = {
      Unit = {
        Description = "AWS credential proxy for Nix daemon";
        Documentation = "https://github.com/Dauliac/aws-nix-cache";
      };

      Service = {
        ExecStart =
          let
            args = lib.concatStringsSep " " (
              [ "${cfg.package}/bin/aws-nix-cache" "serve" "--socket" socketPath ]
              ++ lib.optional (cfg.awsProfile != null) "--aws-profile ${cfg.awsProfile}"
            );
          in args;
        Restart = "always";
        RestartSec = 5;
      };

      Install = {
        WantedBy = [ "default.target" ];
      };
    };
  };
}
