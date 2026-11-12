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
        # The three services: the server (the built frontend is passthru.web, i.e.
        # MODEL_MANAGER_WEB_DIR), the container manager, and the in-container agent + its image.
        packages =
          let
            model-manager = pkgs.callPackage ./nix/package.nix { };
            manage-agent = pkgs.callPackage ./nix/manage-agent.nix { };
            normalize-agent = pkgs.callPackage ./nix/normalize-agent.nix { };
          in
          {
            default = model-manager;
            inherit model-manager manage-agent normalize-agent;
          }
          // nixpkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
            # OCI (docker archive) image; services.modelManager.normalizer imports it into Incus.
            normalize-agent-image = pkgs.callPackage ./nix/normalize-agent-image.nix { inherit normalize-agent; };
          };

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
            echo "  cargo run -p model-manager   # API on http://127.0.0.1:8087"
            echo "  (cd web && pnpm i && pnpm dev)   # UI on http://127.0.0.1:5173"
          '';
        };
      }
    );
}
