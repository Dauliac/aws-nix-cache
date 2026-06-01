{ ... }: {
  perSystem = { config, pkgs, ... }: {
    devShells.default = pkgs.mkShell {
      inputsFrom = [
        config.devShells.rust
      ];

      nativeBuildInputs = with pkgs; [
        cargo-watch
        cargo-nextest
        bacon
      ];
    };
  };
}
