# OCI image (docker archive) for the normalize agent: the agent binary plus every tool its prompt
# promises. The NixOS module serves it to Incus as an OCI image (see manage_agent/module.nix).
#
# The entrypoint is a `sleep`: the manager starts the container and then `incus exec`s the agent into it.
{
  lib,
  dockerTools,
  buildEnv,
  runCommand,
  cacert,
  bash,
  coreutils,
  findutils,
  gnugrep,
  gnused,
  gawk,
  gnutar,
  gzip,
  bzip2,
  xz,
  zstd,
  lz4,
  unzip,
  zip,
  libarchive,
  _7zz,
  file,
  less,
  curl,
  wget,
  git,
  git-lfs,
  jq,
  sqlite,
  duckdb,
  ffmpeg-headless,
  awscli2,
  python3,
  normalize-agent,
}:

let
  python = python3.withPackages (
    ps: with ps; [
      pyarrow
      pandas
      polars
      duckdb
      pillow
      numpy
      openpyxl
      pyyaml
      requests
      huggingface-hub
      datasets
      boto3
      soundfile
      kaggle
    ]
  );

  # `7z` is what the agent's prompt (and extract_archive's fallback) call.
  sevenZip = runCommand "7z-alias" { } ''
    mkdir -p $out/bin
    ln -s ${_7zz}/bin/7zz $out/bin/7z
  '';

  skeleton = runCommand "normalize-agent-skeleton" { } ''
    mkdir -p $out/sbin $out/etc/ssl/certs
    cat > $out/sbin/init <<EOF
    #!${bash}/bin/bash
    exec ${coreutils}/bin/sleep infinity
    EOF
    chmod +x $out/sbin/init
    ln -s ${cacert}/etc/ssl/certs/ca-bundle.crt $out/etc/ssl/certs/ca-certificates.crt
    ln -s ${cacert}/etc/ssl/certs/ca-bundle.crt $out/etc/ssl/certs/ca-bundle.crt
  '';

  root = buildEnv {
    name = "normalize-agent-root";
    pathsToLink = [
      "/bin"
      "/sbin"
      "/etc"
    ];
    paths = [
      skeleton
      dockerTools.fakeNss
      bash
      coreutils
      findutils
      gnugrep
      gnused
      gawk
      gnutar
      gzip
      bzip2
      xz
      zstd
      lz4
      unzip
      zip
      libarchive # bsdtar
      sevenZip
      file
      less
      curl
      wget
      git
      git-lfs
      jq
      sqlite
      duckdb
      ffmpeg-headless
      awscli2
      python
      normalize-agent
    ];
  };
in
dockerTools.buildLayeredImage {
  name = "normalize-agent";
  tag = "latest";
  contents = [ root ];
  extraCommands = ''
    mkdir -p -m 1777 tmp
    mkdir -p root data/raw data/out usr/bin
    ln -s ${coreutils}/bin/env usr/bin/env
  '';
  config = {
    Cmd = [ "/sbin/init" ];
    Env = [
      "PATH=/bin"
      "SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt"
      "LANG=C.UTF-8"
      "HOME=/root"
    ];
    WorkingDir = "/";
  };
}
