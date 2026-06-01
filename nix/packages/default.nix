{ inputs, ... }: {
  imports = [
    inputs.rust-flake.flakeModules.default
  ];

  perSystem = { config, pkgs, ... }: {
    rust-project.defaults.perCrate.crane.args = {
      doCheck = false;
      nativeBuildInputs = with pkgs; [
        pkg-config
      ];
      buildInputs = with pkgs; [
      ] ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
        pkgs.apple-sdk_15
      ];
    };

    packages.default = config.packages.aws-nix-cache;
  };
}
