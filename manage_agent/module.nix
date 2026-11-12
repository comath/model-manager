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
  # `local:normalize-agent` -> the alias the image is imported under.
  imageAlias = lib.removePrefix "local:" cfg.image;
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
        Convert `imagePackage` to an Incus image and import it under the alias in `image`
        whenever the package changes. Disable to supply the image yourself.
      '';
    };

    bind = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1:8088";
      description = "Listen address; model-manager's datasetHook points here.";
    };

    image = lib.mkOption {
      type = lib.types.str;
      default = "local:normalize-agent";
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

    # OCI -> Incus: unpack the docker archive's layers into a rootfs and import it as a container
    # image. (The image's PID 1 just sleeps; manage-agent `incus exec`s the agent into it.)
    systemd.services.normalize-agent-image = lib.mkIf cfg.importImage {
      description = "Import the normalize-agent OCI image into Incus";
      wantedBy = [ "multi-user.target" ];
      wants = [ "incus.service" ];
      after = [ "incus.service" ];
      before = [ "normalize-agent.service" ];
      path = [
        incusPkg
        pkgs.skopeo
        pkgs.umoci
        pkgs.gnutar
        pkgs.coreutils
      ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        StateDirectory = "normalize-agent";
      };
      script = ''
        stamp=/var/lib/normalize-agent/image-stamp
        if [ "$(cat "$stamp" 2>/dev/null)" = ${lib.escapeShellArg "${cfg.imagePackage}"} ] \
           && incus image info ${lib.escapeShellArg imageAlias} >/dev/null 2>&1; then
          exit 0
        fi
        work=$(mktemp -d -p /var/lib/normalize-agent image.XXXXXX)
        trap 'rm -rf "$work"' EXIT
        skopeo copy docker-archive:${cfg.imagePackage} oci:"$work/oci":latest
        umoci unpack --image "$work/oci":latest "$work/bundle"
        cat > "$work/bundle/metadata.yaml" <<EOF
        architecture: ${pkgs.stdenv.hostPlatform.parsed.cpu.name}
        creation_date: $(date +%s)
        properties:
          description: normalize-agent (Nix-built OCI image)
          os: nixos
        EOF
        tar -C "$work/bundle" --numeric-owner -cf "$work/image.tar" metadata.yaml rootfs
        incus image delete ${lib.escapeShellArg imageAlias} 2>/dev/null || true
        incus image import "$work/image.tar" --alias ${lib.escapeShellArg imageAlias}
        echo -n ${lib.escapeShellArg "${cfg.imagePackage}"} > "$stamp"
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
