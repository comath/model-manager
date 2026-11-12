# The cargo workspace as a store path: the root manifest and lockfile plus every member crate
# (cargo needs all member manifests present, whichever package is being built).
{ lib }:
let
  root = ../.;
  crates = map (c: root + "/${c}") [
    "model-manager"
    "manage_agent"
    "normalize_agent"
  ];
in
lib.fileset.toSource {
  inherit root;
  fileset = lib.fileset.difference (lib.fileset.unions (
    [
      (root + "/Cargo.toml")
      (root + "/Cargo.lock")
    ]
    ++ crates
  )) (lib.fileset.unions (map (c: lib.fileset.maybeMissing (c + "/target")) crates));
}
