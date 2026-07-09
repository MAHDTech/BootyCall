{ pkgs, self }:

let
  inherit (pkgs) lib;
  # Narrow the source to only the trees the install phase actually consumes:
  # the checked-in tftpboot placeholders and the static wallpapers. With
  # `src = ../.` any unrelated change — a Rust file, a doc, CI config — altered
  # the derivation's input hash and forced a full rebuild. Scoping src to these
  # two directories means only changes under them bust the assets cache.
  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../tftpboot
      ../static
    ];
  };
in
pkgs.stdenv.mkDerivation {
  name = "bootycall-assets";
  inherit src;
  installPhase = ''
    mkdir -p $out/tftpboot/boot/x64
    mkdir -p $out/tftpboot/boot/arm64
    mkdir -p $out/static

    # Copy checked-in tftpboot files if they exist
    if [ -d "tftpboot" ] && [ "$(ls -A tftpboot)" ]; then
      cp -r tftpboot/* $out/tftpboot/
    fi

    # Copy checked-in static files if they exist
    if [ -d "static" ] && [ "$(ls -A static)" ]; then
      cp -r static/* $out/static/
    fi

    # Overlay compiled iPXE binaries from other packages in the same flake system
    cp -f ${self.packages.${pkgs.stdenv.hostPlatform.system}.ipxe-amd64}/* $out/tftpboot/boot/x64/
    cp -f ${self.packages.${pkgs.stdenv.hostPlatform.system}.ipxe-arm64}/* $out/tftpboot/boot/arm64/
  '';
}
