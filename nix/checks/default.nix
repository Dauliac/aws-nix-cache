{ ... }: {
  perSystem = { config, pkgs, ... }: {
    checks = {
      formatting = pkgs.runCommand "aws-nix-cache-fmt-check" {
        nativeBuildInputs = [ config.rust-project.toolchain ];
        src = config.rust-project.src;
      } ''
        cd $src
        cargo fmt --check
        touch $out
      '';
    };
  };
}
