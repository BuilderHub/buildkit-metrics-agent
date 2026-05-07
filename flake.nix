{
  description = "BuildKit reporting agent";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };
        toolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [
            "rust-src"
            "llvm-tools-preview"
          ];
          targets = [ "x86_64-unknown-linux-musl" ];
        };

        agent = pkgs.rustPlatform.buildRustPackage {
          pname = "buildkit-metrics-agent";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          nativeBuildInputs = with pkgs; [ pkg-config ];
          buildInputs = with pkgs; [ openssl ];
        };
      in
      {
        packages = {
          default = agent;
        } // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          containerImage = pkgs.dockerTools.buildLayeredImage {
            name = "buildkit-metrics-agent";
            contents = [
              agent
              pkgs.cacert
            ];
            config = {
              Entrypoint = [ "${agent}/bin/buildkit-metrics-agent" ];
              Env = [
                "BUILDKIT_ADDR=unix:///run/buildkit/buildkitd.sock"
                "METRICS_ADDR=0.0.0.0:9090"
              ];
              ExposedPorts = {
                "9090/tcp" = { };
              };
              Labels = {
                "org.opencontainers.image.source" = "https://github.com/builderhub/buildkit-metrics-agent";
                "org.opencontainers.image.description" = "A lightweight Rust application that scrapes and exposes BuildKit metrics.";
                "org.opencontainers.image.licenses" = "MIT";
                "org.opencontainers.image.authors" = "BuilderHub";
                "org.opencontainers.image.url" = "https://github.com/builderhub/buildkit-metrics-agent";
                "org.opencontainers.image.documentation" = "https://github.com/builderhub/buildkit-metrics-agent/blob/main/README.md";
                "org.opencontainers.image.vendor" = "BuilderHub";
              };
            };
          };
        };
        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            toolchain
            cargo-llvm-cov
            protobuf
            pkg-config
            openssl
          ];
          env = {
            RUST_BACKTRACE = "1";
          };
        };
      }
    );
}
