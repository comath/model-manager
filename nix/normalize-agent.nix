# normalize-agent: the LLM agent that runs inside the container (`normalize-dataset`).
{
  lib,
  rustPlatform,
  callPackage,
}:

rustPlatform.buildRustPackage {
  pname = "normalize-agent";
  version = (lib.importTOML ../normalize_agent/Cargo.toml).package.version;
  src = callPackage ./workspace-src.nix { };
  cargoLock.lockFile = ../Cargo.lock;
  cargoBuildFlags = [
    "-p"
    "normalize-agent"
  ];
  cargoTestFlags = [
    "-p"
    "normalize-agent"
  ];
  meta = {
    description = "Agent that normalizes one dataset into parquet + README.md + schema.md";
    mainProgram = "normalize-dataset";
  };
}
