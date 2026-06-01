{ inputs, ... }: {
  imports = [
    inputs.rust-flake.flakeModules.nixpkgs
  ];
}
