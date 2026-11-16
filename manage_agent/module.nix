# NixOS module: `services.modelManager.normalizer`, imported by nix/module.nix.
#
# Runs manage-agent next to model-manager and points model-manager's dataset hook
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
  incusPkg = config.virtualisation.incus.clientPackage or config.virtualisation.incus.package;
  registryPort = 5000;
  registryAddr = "127.0.0.1:${toString registryPort}";
  remoteName = "normalize-agent-registry";
  imageRepo = "normalize-agent";
in
{
  options.services.modelManager.normalizer = {
    enable = lib.mkEnableOption "the normalize-agent manager (dataset → parquet agent per download)";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.manage-agent;
      defaultText = lib.literalExpression "model-manager.packages.\${system}.manage-agent";
      description = "The manage-agent package.";
    };

    imagePackage = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.normalize-agent-image;
      defaultText = lib.literalExpression "model-manager.packages.\${system}.normalize-agent-image";
      description = "The Nix-built OCI image (docker archive) the agent runs in.";
    };

    importImage = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = ''
        Serve `imagePackage` from a loopback registry (pushed again whenever the package changes)
        and register it as the Incus OCI remote that `image` points at. Disable to supply the
        image yourself.
      '';
    };

    bind = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1:8088";
      description = "Listen address; model-manager's datasetHook points here.";
    };

    image = lib.mkOption {
      type = lib.types.str;
      default = "${remoteName}:${imageRepo}";
      description = "Incus image the agent runs in (must contain `command`). The default is the one `importImage` creates.";
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

    # Incus runs OCI images pulled from a registry (it shells out to skopeo). Serve the Nix-built
    # image from a loopback-only registry and register that as an Incus OCI remote.
    services.dockerRegistry = lib.mkIf cfg.importImage {
      enable = true;
      listenAddress = "127.0.0.1";
      port = registryPort;
    };
    # Incus' skopeo pulls over plain http from the loopback registry.
    virtualisation.containers = lib.mkIf cfg.importImage {
      enable = lib.mkDefault true;
      registries.insecure = [ registryAddr ];
    };

    systemd.services.normalize-agent-image = lib.mkIf cfg.importImage {
      description = "Publish the normalize-agent OCI image to the local registry for Incus";
      wantedBy = [ "multi-user.target" ];
      wants = [ "incus.service" ];
      after = [
        "incus.service"
        "docker-registry.service"
      ];
      requires = [ "docker-registry.service" ];
      before = [ "normalize-agent.service" ];
      path = [
        incusPkg
        pkgs.skopeo
        pkgs.coreutils
      ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        StateDirectory = "normalize-agent";
      };
      script = ''
        if ! incus remote list --format csv | cut -d, -f1 | grep -qx ${lib.escapeShellArg remoteName}; then
          # The URL's scheme is not used for pulls: Incus always runs `skopeo ... docker://host/image`.
          incus remote add ${lib.escapeShellArg remoteName} https://${registryAddr} --protocol=oci
        fi
        stamp=/var/lib/normalize-agent/image-stamp
        if [ "$(cat "$stamp" 2>/dev/null)" != ${lib.escapeShellArg "${cfg.imagePackage}"} ]; then
          skopeo copy --dest-tls-verify=false \
            docker-archive:${cfg.imagePackage} docker://${registryAddr}/${imageRepo}:latest
          echo -n ${lib.escapeShellArg "${cfg.imagePackage}"} > "$stamp"
        fi
      '';
    };

    systemd.services.normalize-agent = {
      description = "normalize-agent (dataset → parquet agent per model-manager download)";
      wantedBy = [ "multi-user.target" ];
      wants = [ "incus.service" ];
      after = [
        "incus.service"
        "model-manager.service"
      ]
      ++ lib.optional cfg.importImage "normalize-agent-image.service";
      requires = lib.optional cfg.importImage "normalize-agent-image.service";
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
            "${incusPkg}/bin/incus"
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
