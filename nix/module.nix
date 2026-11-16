# NixOS module: `services.modelManager`. Exported as `nixosModules.default`; `self` is this
# flake, so the package defaults to the one built here.
#
# Runs as the unprivileged `models` service account, which owns the store roots. There is no
# auth: bind it where only trusted networks (e.g. a firewall-trusted tailscale0) can reach it.
self:
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.modelManager;

  # Store roots are often separate filesystems (ZFS datasets, mounts); don't create them here,
  # where a missing mount would silently fill the parent filesystem instead.
  prepareStores = pkgs.writeShellScript "model-manager-prepare-stores" ''
    missing=0
    ${lib.concatMapStrings (s: ''
      if [ -d ${lib.escapeShellArg (toString s.path)} ]; then
        ${pkgs.coreutils}/bin/chown ${cfg.user}:${cfg.user} ${lib.escapeShellArg (toString s.path)}
      else
        echo "store ${s.name}: ${toString s.path} does not exist (filesystem not mounted or not created?)" >&2
        missing=1
      fi
    '') cfg.stores}
    # Datasets: hand an existing folder to the service account; otherwise the service creates it.
    if [ -d ${lib.escapeShellArg (toString cfg.datasetsDir)} ]; then
      ${pkgs.coreutils}/bin/chown ${cfg.user}:${cfg.user} ${lib.escapeShellArg (toString cfg.datasetsDir)}
    fi
    exit $missing
  '';
in
{
  imports = [ (import ../manage_agent/module.nix self) ];

  options.services.modelManager = {
    enable = lib.mkEnableOption "model-manager, the Hugging Face model downloader";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "model-manager.packages.\${system}.default";
      description = "The model-manager package; `passthru.web` is the built frontend.";
    };

    stores = lib.mkOption {
      type = lib.types.listOf (
        lib.types.submodule {
          options = {
            name = lib.mkOption {
              type = lib.types.strMatching "[A-Za-z0-9_-]+";
              description = "Name shown in the UI's store picker.";
            };
            path = lib.mkOption {
              type = lib.types.path;
              description = "Root directory; models land in <path>/<kind>/<owner>/<name>.";
            };
          };
        }
      );
      description = "Where models can be downloaded to (MODEL_MANAGER_STORES). The first is the default.";
    };

    defaultStore = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "slow";
      description = "Store used when none is picked (MODEL_MANAGER_DEFAULT_STORE). Null keeps the app's own default: the store named `slow` if there is one, else the first.";
    };

    parallelFiles = lib.mkOption {
      type = lib.types.nullOr lib.types.ints.positive;
      default = null;
      description = "Files fetched at once within one job (MODEL_MANAGER_PARALLEL_FILES); null uses the app default (2).";
    };

    containerRuntime = lib.mkOption {
      type = lib.types.enum [
        "podman"
        "docker"
      ];
      default = "podman";
      description = "Container CLI used to launch model runtimes (MODEL_MANAGER_CONTAINER_RUNTIME).";
    };

    tailscaleBind = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "100.64.0.1:8088";
      description = "Extra unauthenticated plain-HTTP listen address, e.g. on the tailscale interface (MODEL_MANAGER_TAILSCALE_BIND).";
    };

    tailscaleServe = {
      enable = lib.mkEnableOption "publishing the service on the tailnet with `tailscale serve` (use instead of `tls`)";
      service = lib.mkOption {
        type = lib.types.str;
        default = "svc:model-manager";
        example = "svc:models";
        description = "Tailscale Service to publish as (https://<name>.<tailnet>.ts.net). Must be defined in the admin console, with this host allowed to host it.";
      };
      httpsPort = lib.mkOption {
        type = lib.types.port;
        default = 443;
        description = "HTTPS port Tailscale listens on for the Service.";
      };
      target = lib.mkOption {
        type = lib.types.str;
        default = "http://${cfg.bind}";
        defaultText = lib.literalExpression ''"http://''${config.services.modelManager.bind}"'';
        description = "Backend Tailscale proxies to. Keep `bind` on loopback so only tailscaled can reach it.";
      };
    };

    tls = {
      cert = lib.mkOption {
        type = lib.types.nullOr lib.types.path;
        default = null;
        description = "PEM server certificate (MODEL_MANAGER_TLS_CERT); needs `tls.key`.";
      };
      key = lib.mkOption {
        type = lib.types.nullOr lib.types.path;
        default = null;
        description = "PEM server private key (MODEL_MANAGER_TLS_KEY). Pass a path string, not a store path, so the key stays out of the Nix store.";
      };
      clientCa = lib.mkOption {
        type = lib.types.nullOr lib.types.path;
        default = null;
        description = "PEM CA that client certificates must chain to; enables mTLS (MODEL_MANAGER_CLIENT_CA).";
      };
    };

    datasetsDir = lib.mkOption {
      type = lib.types.path;
      default = "/var/lib/model-manager/datasets";
      description = "Where datasets are downloaded (MODEL_MANAGER_DATASETS_DIR); laid out as <path>/<source>/<owner>/<name>.";
    };

    bind = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1:8087";
      description = "Listen address (MODEL_MANAGER_BIND).";
    };

    datasetHook = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "http://127.0.0.1:8088/jobs";
      description = "URL POSTed {source, repo_id, revision, path} after each completed dataset download (MODEL_MANAGER_DATASET_HOOK). Set automatically by `normalizer.enable`.";
    };

    environmentFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = "Optional file with HF_TOKEN=…, KAGGLE_USERNAME=…/KAGGLE_KEY=…, GITHUB_TOKEN=… for gated/private repos.";
    };

    user = lib.mkOption {
      type = lib.types.str;
      default = "models";
      description = "Service account; created (with a group of the same name) by this module.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.stores != [ ];
        message = "services.modelManager.stores must list at least one store.";
      }
      {
        assertion = cfg.defaultStore == null || lib.any (s: s.name == cfg.defaultStore) cfg.stores;
        message = "services.modelManager.defaultStore must name one of services.modelManager.stores.";
      }
      {
        assertion = (cfg.tls.cert == null) == (cfg.tls.key == null);
        message = "services.modelManager.tls.cert and tls.key must be set together.";
      }
      {
        assertion = !cfg.tailscaleServe.enable || config.services.tailscale.enable;
        message = "services.modelManager.tailscaleServe needs services.tailscale.enable = true.";
      }
      {
        assertion = !cfg.tailscaleServe.enable || cfg.tls.cert == null;
        message = "services.modelManager.tailscaleServe terminates HTTPS itself; unset tls.* (the backend must be plain HTTP).";
      }
      {
        assertion = cfg.tls.clientCa == null || cfg.tls.cert != null;
        message = "services.modelManager.tls.clientCa requires tls.cert and tls.key.";
      }
    ];

    users.groups.${cfg.user} = { };
    users.users.${cfg.user} = {
      isSystemUser = true;
      group = cfg.user;
      description = "model-manager service account (owns the model stores)";
      home = "/var/lib/model-manager";
    };

    # `tailscale serve --bg` stores its config in tailscaled, and repeating the same command just
    # replaces it, so running this on every boot/rebuild is safe. It fails until tailscaled is
    # logged in, hence the retry.
    systemd.services.model-manager-tailscale-serve = lib.mkIf cfg.tailscaleServe.enable {
      description = "Publish model-manager on the tailnet (tailscale serve)";
      wantedBy = [ "multi-user.target" ];
      wants = [ "tailscaled.service" ];
      after = [
        "tailscaled.service"
        "model-manager.service"
      ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        Restart = "on-failure";
        RestartSec = 10;
        ExecStart = lib.escapeShellArgs [
          (lib.getExe config.services.tailscale.package)
          "serve"
          "--bg"
          "--service=${cfg.tailscaleServe.service}"
          "--https=${toString cfg.tailscaleServe.httpsPort}"
          cfg.tailscaleServe.target
        ];
      };
    };

    systemd.services.model-manager = {
      description = "model-manager (Hugging Face model downloads)";
      wantedBy = [ "multi-user.target" ];
      wants = [ "network-online.target" ];
      after = [ "network-online.target" ];
      unitConfig.RequiresMountsFor = map (s: s.path) cfg.stores;

      environment = {
        MODEL_MANAGER_STORES = lib.concatMapStringsSep "," (s: "${s.name}=${s.path}") cfg.stores;
        MODEL_MANAGER_DATASETS_DIR = toString cfg.datasetsDir;
        MODEL_MANAGER_BIND = cfg.bind;
        MODEL_MANAGER_CONTAINER_RUNTIME = cfg.containerRuntime;
        MODEL_MANAGER_DATA_DIR = "/var/lib/model-manager";
        MODEL_MANAGER_WEB_DIR = "${cfg.package.web}";
        HOME = "/var/lib/model-manager";
        RUST_LOG = "model_manager=info,tower_http=warn";
      }
      // lib.optionalAttrs (cfg.datasetHook != null) {
        MODEL_MANAGER_DATASET_HOOK = cfg.datasetHook;
      }
      // lib.optionalAttrs (cfg.defaultStore != null) {
        MODEL_MANAGER_DEFAULT_STORE = cfg.defaultStore;
      }
      // lib.optionalAttrs (cfg.parallelFiles != null) {
        MODEL_MANAGER_PARALLEL_FILES = toString cfg.parallelFiles;
      }
      // lib.optionalAttrs (cfg.tailscaleBind != null) {
        MODEL_MANAGER_TAILSCALE_BIND = cfg.tailscaleBind;
      }
      // lib.optionalAttrs (cfg.tls.cert != null) {
        MODEL_MANAGER_TLS_CERT = toString cfg.tls.cert;
        MODEL_MANAGER_TLS_KEY = toString cfg.tls.key;
      }
      // lib.optionalAttrs (cfg.tls.clientCa != null) {
        MODEL_MANAGER_CLIENT_CA = toString cfg.tls.clientCa;
      };

      serviceConfig = {
        User = cfg.user;
        Group = cfg.user;
        StateDirectory = "model-manager";
        EnvironmentFile = lib.optional (cfg.environmentFile != null) cfg.environmentFile;
        # Check the store roots exist and hand them to the service account (not recursive:
        # existing models keep their owner). "+" runs this one step as root.
        ExecStartPre = "+${prepareStores}";
        ExecStart = lib.getExe cfg.package;
        Restart = "on-failure";
        RestartSec = 30;
        # Downloads stay world-readable (e.g. for read-only NFS exports squashed to nobody).
        UMask = "0022";

        ProtectSystem = "strict";
        # "-": a missing store must not abort namespace setup, or prepareStores never gets to
        # say which one is missing (the unit would just fail with 226/NAMESPACE).
        ReadWritePaths = map (p: "-${toString p}") (map (s: s.path) cfg.stores ++ [ cfg.datasetsDir ]);
        ProtectHome = true;
        PrivateTmp = true;
        PrivateDevices = true;
        NoNewPrivileges = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectControlGroups = true;
        RestrictSUIDSGID = true;
        LockPersonality = true;
      };
    };
  };
}
