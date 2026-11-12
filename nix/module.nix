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
    ];

    users.groups.${cfg.user} = { };
    users.users.${cfg.user} = {
      isSystemUser = true;
      group = cfg.user;
      description = "model-manager service account (owns the model stores)";
      home = "/var/lib/model-manager";
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
        MODEL_MANAGER_DATA_DIR = "/var/lib/model-manager";
        MODEL_MANAGER_WEB_DIR = "${cfg.package.web}";
        HOME = "/var/lib/model-manager";
        RUST_LOG = "model_manager=info,tower_http=warn";
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
