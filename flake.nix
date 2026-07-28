{
  description = "SCD30 and SEN66 sensor monitor with GreptimeDB storage";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
      flake-utils,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };

        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          targets = [ "aarch64-unknown-linux-gnu" ];
        };

        # Native build for development/testing
        mkPackage =
          pkgs:
          pkgs.rustPlatform.buildRustPackage {
            pname = "sensor_monitor";
            version = "0.1.0";
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;

            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs = [
              pkgs.openssl
              pkgs.sqlite
            ];

            meta = {
              description = "SCD30 and SEN66 monitor with durable GreptimeDB storage";
              license = pkgs.lib.licenses.mit;
              mainProgram = "sensor_monitor";
            };
          };
      in
      {
        packages = {
          default = mkPackage pkgs;
          native = mkPackage pkgs;
          rpi = mkPackage pkgs.pkgsCross.aarch64-multiplatform.pkgsStatic;
        };

        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            cargo
            rust-analyzer
            rustfmt
            pkg-config
            openssl
            sqlite
            sqlx-cli
            postgresql
          ];
        };
      }
    );
}
