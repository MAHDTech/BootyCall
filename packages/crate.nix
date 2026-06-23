{ pkgs }:
name:
pkgs.rustPlatform.buildRustPackage {
  pname = name;
  version = "0.1.0";
  src = ../.;
  cargoLock = {
    lockFile = ../Cargo.lock;
  };
  cargoBuildFlags = [
    "-p"
    name
  ];
  doCheck = false;
}
