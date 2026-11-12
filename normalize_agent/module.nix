# NixOS module: `services.modelManager.normalizer`, imported by nix/module.nix.
#
# Runs the normalize-agent manager next to model-manager and points model-manager's dataset hook
# at it. It runs as root: it drives Incus (incus-admin is root-equivalent anyway) and hands the
# agent's output, written by the container's root, over to the models account.
self:
{
  config,
  lib,
  pkgs,
  ...
}:

let
  mm = config.services.modelManager;
  cfg = mm.normalizer;
  normalizedDir = "${toString mm.datasetsDir}/normalized";
in
{
  options.services.modelManager.normalizer = {
    enable = lib.mkEnableOption "the normalize-agent manager (dataset → parquet agent per download)";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.normalize-agent;
      defaultText = lib.literalExpression "model-manager.packages.\${system}.normalize-agent";
      description = "The normalize-agent manager package.";
    };

    bind = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1:8088";
      description = "Listen address; model-manager's datasetHook points here.";
    };

    image = lib.mkOption {
      type = lib.types.str;
      example = "local:normalize-agent";
      description = "Incus image the agent runs in (must contain `command`).";
    };

    command = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ "normalize-dataset" ];
      description = "Agent command inside the container; must not contain spaces per argument.";
    };

    profiles = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = "Incus profiles for the agent containers (network, ACLs). Empty: Incus' default profile.";
    };

    containerConfig = lib.mkOption {
      type = lib.types.attrsOf lib.types.str;
      default = {
        "limits.cpu" = "8";
        "limits.memory" = "32GiB";
      };
      description = "Incus instance config for each agent container.";
    };

    environment = lib.mkOption {
      type = lib.types.attrsOf lib.types.str;
      default = { };
      example = {
        OPENAI_BASE_URL = "http://spark-1:8000/v1";
        OPENAI_MODEL = "gemma-4-31b";
      };
      description = "Non-secret environment for the agent.";
    };

    environmentFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = "Secrets for the manager's environment; list the ones the agent gets in `forwardEnvironment`.";
    };

    forwardEnvironment = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [ "HF_TOKEN" ];
      description = "Variables from `environmentFile` passed on to the agent.";
    };

    timeout = lib.mkOption {
      type = lib.types.ints.positive;
      default = 4 * 3600;
      description = "Seconds an agent run may take before its container is killed.";
    };

    workers = lib.mkOption {
      type = lib.types.ints.positive;
      default = 1;
      description = "Agents running at once.";
    };
  };

  config = lib.mkIf (mm.enable && cfg.enable) {
    assertions = [
      {
        assertion = config.virtualisation.incus.enable;
        message = "services.modelManager.normalizer needs virtualisation.incus.enable.";
      }
    ];

    services.modelManager.datasetHook = lib.mkDefault "http://${cfg.bind}/jobs";

    systemd.services.normalize-agent = {
      description = "normalize-agent (dataset → parquet agent per model-manager download)";
      wantedBy = [ "multi-user.target" ];
      wants = [ "incus.service" ];
      after = [
        "incus.service"
        "model-manager.service"
      ];
      unitConfig.RequiresMountsFor = [ (toString mm.datasetsDir) ];
      environment.HOME = "/var/lib/normalize-agent";

      serviceConfig = {
        # Before the namespace is set up, so ReadWritePaths has something to bind.
        ExecStartPre = "+${pkgs.coreutils}/bin/mkdir -p ${lib.escapeShellArg normalizedDir}";
        ExecStart = lib.escapeShellArgs (
          [
            (lib.getExe cfg.package)
            "--bind"
            cfg.bind
            "--datasets-root"
            (toString mm.datasetsDir)
            "--image"
            cfg.image
            "--command"
            (lib.concatStringsSep " " cfg.command)
            "--timeout"
            (toString cfg.timeout)
            "--workers"
            (toString cfg.workers)
            "--owner"
            "${mm.user}:${mm.user}"
            "--incus"
            "${config.virtualisation.incus.clientPackage or config.virtualisation.incus.package}/bin/incus"
          ]
          ++ lib.concatMap (p: [
            "--profile"
            p
          ]) cfg.profiles
          ++ lib.concatLists (
            lib.mapAttrsToList (k: v: [
              "--config"
              "${k}=${v}"
            ]) cfg.containerConfig
          )
          ++ lib.concatLists (
            lib.mapAttrsToList (k: v: [
              "--env"
              "${k}=${v}"
            ]) cfg.environment
          )
          ++ lib.concatMap (n: [
            "--forward-env"
            n
          ]) cfg.forwardEnvironment
        );
        EnvironmentFile = lib.optional (cfg.environmentFile != null) cfg.environmentFile;
        StateDirectory = "normalize-agent";
        Restart = "on-failure";
        RestartSec = 30;

        # Writes only under <datasets>/normalized; raw downloads stay read-only to it too.
        ProtectSystem = "strict";
        ReadWritePaths = [ normalizedDir ];
        ProtectHome = true;
        PrivateTmp = true;
        PrivateDevices = true;
        NoNewPrivileges = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectControlGroups = true;
        RestrictSUIDSGID = true;
        LockPersonality = true;
        RestrictAddressFamilies = [
          "AF_UNIX"
          "AF_INET"
          "AF_INET6"
        ];
      };
    };
  };
}
