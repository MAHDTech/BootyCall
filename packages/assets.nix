{ pkgs, self }:

pkgs.stdenv.mkDerivation {
  name = "bootycall-assets";
  src = ../.;
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
