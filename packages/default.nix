{
  pkgs,
  self,
  ...
}:

let
  bootycall = pkgs.callPackage ./crate.nix { } "bootycall-rs";
in
{
  inherit bootycall;

  ipxe-amd64 = pkgs.callPackage ./ipxe/amd64.nix { inherit self; };

  ipxe-arm64 = pkgs.callPackage ./ipxe/arm64.nix { inherit self; };

  assets = pkgs.callPackage ./assets.nix { inherit self; };

  default = bootycall;
}
