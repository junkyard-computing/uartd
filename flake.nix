{
  description = "uartd: buffered UART console daemon + CLI for AI-driven serial control";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      # uartd is a HOST-side tool (unlike the on-device pixel-* binaries), so this is a
      # native build, not an aarch64-musl cross.
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAll = nixpkgs.lib.genAttrs systems;
      pkgsFor = system: import nixpkgs { inherit system; };

      package = system:
        let pkgs = pkgsFor system;
        in pkgs.rustPlatform.buildRustPackage {
          pname = "uartd";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          # The integration tests need a pty + the network socket; skip them in the sandbox
          # (they run under `cargo test` / `nix flake check`'s devShell instead).
          doCheck = false;
          meta = {
            description = "Buffered UART console daemon + CLI for AI-driven serial control";
            license = pkgs.lib.licenses.asl20;
            mainProgram = "uart";
          };
        };
    in {
      packages = forAll (system: rec {
        uartd = package system;
        default = uartd;
      });

      apps = forAll (system: {
        uartd = {
          type = "app";
          program = "${self.packages.${system}.uartd}/bin/uartd";
          meta.description = "uartd serial console daemon";
        };
        uart = {
          type = "app";
          program = "${self.packages.${system}.uartd}/bin/uart";
          meta.description = "uart CLI client for uartd";
        };
      });

      devShells = forAll (system:
        let pkgs = pkgsFor system;
        in {
          default = pkgs.mkShell {
            packages = [ pkgs.cargo pkgs.rustc pkgs.rustfmt pkgs.clippy ];
          };
        });
    };
}
