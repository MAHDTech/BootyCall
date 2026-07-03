{ pkgs, ... }:

let
  targetPkgs = if pkgs.stdenv.hostPlatform.isx86_64 then pkgs else pkgs.pkgsCross.gnu64;
in
targetPkgs.callPackage ./default.nix { } {
  pname = "ipxe-amd64";
  targets = [
    "bin-x86_64-efi/ipxe.efi"
    "bin-x86_64-efi/snp.efi"
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
