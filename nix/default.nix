{ ... }: {
  imports = [
    ./overlays
    ./packages
    ./devshells
    ./checks
  ];

  flake.nixosModules.default = ./nixos;
  flake.homeManagerModules.default = ./home-manager;
}
