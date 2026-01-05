{
  description = "SCD30 CO2 sensor monitor for Raspberry Pi Zero 2 W";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-24.05";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };

        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          targets = [ "aarch64-unknown-linux-gnu" ];
        };

        # Native build for development/testing
        mkPackage = pkgs: pkgs.rustPlatform.buildRustPackage {
          pname = "sensor_monitor";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = [ pkgs.openssl ];

          meta = {
            description = "SCD30 CO2 sensor monitor with PostgreSQL storage";
            license = pkgs.lib.licenses.mit;
          };
        };
      in {
        packages = {
          default = mkPackage pkgs;
          native = mkPackage pkgs;
          rpi = mkPackage pkgs.pkgsCross.aarch64-multiplatform;
        };

        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            cargo
            rust-analyzer
            rustfmt
            pkg-config
            openssl
            sqlx-cli
            postgresql
          ];
        };
      }
    );
}
