{ pkgs }:
let
  # QUAL-5: read the version from the workspace manifest so the Nix
  # derivation can't drift from Cargo.toml (this file used to hard-code
  # "0.1.0" while the workspace had already moved past that).
  workspaceVersion = (builtins.fromTOML (builtins.readFile ../Cargo.toml)).workspace.package.version;
in
name:
pkgs.rustPlatform.buildRustPackage {
  pname = name;
  version = workspaceVersion;
  src = ../.;
  cargoLock = {
    lockFile = ../Cargo.lock;
  };
  cargoBuildFlags = [
    "-p"
    name
  ];
  doCheck = true;
}
