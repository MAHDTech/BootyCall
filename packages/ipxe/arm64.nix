{ pkgs, ... }:

let
  targetPkgs =
    if pkgs.stdenv.hostPlatform.isAarch64 then pkgs else pkgs.pkgsCross.aarch64-multiplatform;
in
targetPkgs.callPackage ./default.nix { } {
  pname = "ipxe-arm64";
  targets = [
    "bin-arm64-efi/ipxe.efi"
    "bin-arm64-efi/snp.efi"
  ];
  additionalConfig = [
    "CONSOLE_CMD"
    "CONSOLE_FRAMEBUFFER"
    "IMAGE_PNG"
    "REBOOT_CMD"
    "POWEROFF_CMD"
    "NTP_CMD"
    "NSLOOKUP_CMD"
    "DOWNLOAD_PROTO_TFTP"
  ];
}
