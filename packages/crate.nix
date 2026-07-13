{ pkgs }:
let
  inherit (pkgs) lib;
  # QUAL-5: read the version from the workspace manifest so the Nix
  # derivation can't drift from Cargo.toml (this file used to hard-code
  # "0.1.0" while the workspace had already moved past that).
  workspaceVersion = (builtins.fromTOML (builtins.readFile ../Cargo.toml)).workspace.package.version;

  # Build with the pinned Rust toolchain from rust-toolchain.toml so that
  # `nix build` uses the exact same compiler as devenv and CI.
  pinnedToolchain = pkgs.rust-bin.fromRustupToolchainFile ../rust-toolchain.toml;
  pinnedRustPlatform = pkgs.makeRustPlatform {
    cargo = pinnedToolchain;
    rustc = pinnedToolchain;
  };
in
name:
pinnedRustPlatform.buildRustPackage {
  pname = name;
  version = workspaceVersion;
  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../rust-toolchain.toml
      ../crates
    ];
  };
  cargoLock = {
    lockFile = ../Cargo.lock;
  };
  cargoBuildFlags = [
    "-p"
    name
  ];
  doCheck = true;
}
