# manage-agent: HTTP service that runs a normalize-agent container in Incus per dataset download.
{
  lib,
  rustPlatform,
  callPackage,
}:

rustPlatform.buildRustPackage {
  pname = "manage-agent";
  version = (lib.importTOML ../manage_agent/Cargo.toml).package.version;
  src = callPackage ./workspace-src.nix { };
  cargoLock.lockFile = ../Cargo.lock;
  cargoBuildFlags = [
    "-p"
    "manage-agent"
  ];
  cargoTestFlags = [
    "-p"
    "manage-agent"
  ];
  meta = {
    description = "Runs a dataset-normalizing agent in an Incus container per model-manager download";
    mainProgram = "manage-agent";
  };
}
