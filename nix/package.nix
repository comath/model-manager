# model-manager: the Rust server plus the built frontend (`passthru.web`).
#
# When web/pnpm-lock.yaml changes, set `hash` below to lib.fakeHash, build, and copy the hash
# from the error (`nix build .#default`).
{
  lib,
  rustPlatform,
  stdenvNoCC,
  nodejs,
  pnpm_11,
  fetchPnpmDeps,
  pnpmConfigHook,
}:

let
  root = ../.;
  version = (lib.importTOML ../Cargo.toml).package.version;

  # Only what the build needs; keeps target/, node_modules and local builds out of the store.
  webFiles = lib.fileset.difference (root + "/web") (
    lib.fileset.unions [
      (lib.fileset.maybeMissing (root + "/web/node_modules"))
      (lib.fileset.maybeMissing (root + "/web/dist"))
      (lib.fileset.maybeMissing (root + "/web/tsconfig.tsbuildinfo"))
    ]
  );

  web = stdenvNoCC.mkDerivation (finalAttrs: {
    pname = "model-manager-web";
    inherit version;
    src = lib.fileset.toSource {
      root = root + "/web";
      fileset = webFiles;
    };
    pnpmDeps = fetchPnpmDeps {
      inherit (finalAttrs) pname version src;
      pnpm = pnpm_11;
      fetcherVersion = 4;
      hash = "sha256-99o0fgPXA7qK+rebT6B/b0sQLaYHVNfgebtUBz0Oq98=";
    };
    nativeBuildInputs = [
      nodejs
      pnpmConfigHook
      pnpm_11
    ];
    buildPhase = ''
      runHook preBuild
      pnpm build
      runHook postBuild
    '';
    installPhase = ''
      runHook preInstall
      cp -r dist $out
      runHook postInstall
    '';
  });
in
rustPlatform.buildRustPackage {
  pname = "model-manager";
  inherit version;
  src = lib.fileset.toSource {
    inherit root;
    fileset = lib.fileset.unions [
      (root + "/Cargo.toml")
      (root + "/Cargo.lock")
      (root + "/src")
    ];
  };
  cargoLock.lockFile = ../Cargo.lock;

  passthru = { inherit web; };

  meta = {
    description = "Hugging Face model downloader with a web UI";
    homepage = "https://github.com/comath/model-manager";
    mainProgram = "model-manager";
  };
}
