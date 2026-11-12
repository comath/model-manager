# normalize-agent manager: a small Rust service that drives Incus.
{
  lib,
  rustPlatform,
}:

let
  root = ./.;
in
rustPlatform.buildRustPackage {
  pname = "normalize-agent";
  version = (lib.importTOML ./Cargo.toml).package.version;
  src = lib.fileset.toSource {
    inherit root;
    fileset = lib.fileset.unions [
      (root + "/Cargo.toml")
      (root + "/Cargo.lock")
      (root + "/src")
    ];
  };
  cargoLock.lockFile = ./Cargo.lock;
  meta = {
    description = "Runs a dataset-normalizing agent in an Incus container per model-manager download";
    mainProgram = "normalize-agent";
  };
}
