{
  description = "uart tools: buffered UART console daemon (uartd) + delta-flash transport (uartfs)";

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
          # uartfs shells out to zstd on the host for delta patches.
          buildInputs = [ pkgs.zstd ];
          meta = {
            description = "UART console daemon + CLI + delta-flash transport";
            license = pkgs.lib.licenses.asl20;
            mainProgram = "uart";
          };
        };

      # The console front-end runs ON the device, so it cross-builds to a static aarch64-musl
      # binary (same pattern as the pixel-* tools), pushed to the phone by the agentless floor.
      frontendAarch64 = system:
        let
          cross = import nixpkgs {
            inherit system;
            crossSystem = { config = "aarch64-unknown-linux-musl"; };
          };
        in cross.rustPlatform.buildRustPackage {
          pname = "uartfs-frontend";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          cargoBuildFlags = [ "-p" "uartfs-frontend" ];
          doCheck = false;
          RUSTFLAGS = "-C target-feature=+crt-static";
          meta = {
            description = "On-device pty-owning console front-end (static aarch64-musl)";
            license = nixpkgs.lib.licenses.asl20;
            mainProgram = "uartfs-frontend";
          };
        };
    in {
      packages = forAll (system: rec {
        uartd = package system;
        uartfs-frontend-aarch64 = frontendAarch64 system;
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
        uartfs = {
          type = "app";
          program = "${self.packages.${system}.uartd}/bin/uartfs";
          meta.description = "uartfs delta-flash transport CLI";
        };
      });

      devShells = forAll (system:
        let pkgs = pkgsFor system;
        in {
          default = pkgs.mkShell {
            packages = [ pkgs.cargo pkgs.rustc pkgs.rustfmt pkgs.clippy pkgs.zstd ];
          };
        });
    };
}
