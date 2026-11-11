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

    bind = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1:8087";
      description = "Listen address (MODEL_MANAGER_BIND).";
    };

    environmentFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = "Optional file with HF_TOKEN=… for gated/private repos.";
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
        # Hand the store roots to the service account (not recursive: existing models keep
        # their owner). "+" runs this one step as root.
        ExecStartPre = "+${pkgs.coreutils}/bin/chown ${cfg.user}:${cfg.user} ${
          lib.escapeShellArgs (map (s: toString s.path) cfg.stores)
        }";
        ExecStart = lib.getExe cfg.package;
        Restart = "on-failure";
        RestartSec = 5;
        # Downloads stay world-readable (e.g. for read-only NFS exports squashed to nobody).
        UMask = "0022";

        ProtectSystem = "strict";
        ReadWritePaths = map (s: s.path) cfg.stores;
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
