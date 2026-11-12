{
  description = "model-manager — Rust/axum backend + React Router frontend for fetching HuggingFace models";

  inputs = {
    nixpkgs.url = "https://flakehub.com/f/NixOS/nixpkgs/0";
    flake-utils.url = "github:numtide/flake-utils";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      fenix,
      ...
    }:
    {
      # services.modelManager; the package defaults to packages.<system>.default below.
      nixosModules.default = import ./nix/module.nix self;
    }
    // flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};

        # Rust toolchain (stable + rust-analyzer/clippy/rustfmt for editors).
        nativeToolchain = fenix.packages.${system}.stable.completeToolchain;
      in
      {
        # Server binary; the built frontend is passthru.web (MODEL_MANAGER_WEB_DIR).
        packages.default = pkgs.callPackage ./nix/package.nix { };
        # Dataset normalizer manager (services.modelManager.normalizer).
        packages.normalize-agent = pkgs.callPackage ./normalize_agent/package.nix { };

        devShells.default = pkgs.mkShell {
          buildInputs = [
            nativeToolchain

            # Frontend
            pkgs.nodejs_22
            pkgs.pnpm

            # Misc tooling
            pkgs.pkg-config
            pkgs.curl
            pkgs.jq
          ];

          # rustls is used for TLS, so no OpenSSL is needed at build time.
          env = {
            RUST_LOG = "model_manager=debug,tower_http=info";
            RUST_BACKTRACE = "1";
          };

          shellHook = ''
            echo "model-manager dev shell"
            echo "  cargo run            # API on http://127.0.0.1:8087"
            echo "  (cd web && pnpm i && pnpm dev)   # UI on http://127.0.0.1:5173"
          '';
        };
      }
    );
}
