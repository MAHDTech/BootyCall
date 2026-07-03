{
  pkgs,
  self,
  ...
}:

let
  bootycall = pkgs.callPackage ./crate.nix { } "bootycall-rs";
  isLinux = pkgs.stdenv.hostPlatform.isLinux;
in
{
  inherit bootycall;
  default = bootycall;
}
// pkgs.lib.optionalAttrs isLinux {
  ipxe-amd64 = pkgs.callPackage ./ipxe/amd64.nix { inherit self; };

  ipxe-arm64 = pkgs.callPackage ./ipxe/arm64.nix { inherit self; };

  assets = pkgs.callPackage ./assets.nix { inherit self; };
}
