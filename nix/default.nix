{ ... }: {
  imports = [
    ./overlays
    ./packages
    ./devshells
    ./checks
  ];

  flake.nixosModules.default = ./nixos;
}
