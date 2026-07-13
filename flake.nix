{
  description = "BootyCall Unified Network Boot Service";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    devenv.url = "github:cachix/devenv";
    devenv.inputs.nixpkgs.follows = "nixpkgs";
    systems.url = "github:nix-systems/default";
    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    {
      self,
      nixpkgs,
      devenv,
      systems,
      ...
    }@inputs:
    let
      forEachSystem = nixpkgs.lib.genAttrs (import systems);
    in
    {
      # NixOS configurations and services modules
      nixosModules = import ./modules self;

      # Package definitions for all systems
      packages = forEachSystem (
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ inputs.rust-overlay.overlays.default ];
          };
        in
        import ./packages { inherit pkgs self system; }
      );

      # VM tests / checks using runNixOSTest
      checks = forEachSystem (
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ inputs.rust-overlay.overlays.default ];
          };
        in
        pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          vmTest = pkgs.testers.runNixOSTest {
            name = "bootycall-init-test";
            nodes.server = {
              imports = [ self.nixosModules.default ];
              services.bootycall.enable = true;
            };
            testScript = ''
              # Wait for the service to start initially
              server.wait_for_unit("bootycall.service")

              # Check that default assets are seeded
              server.succeed("test -f /var/lib/bootycall/tftpboot/boot/x64/ipxe.efi")

              # Stop the service
              server.systemctl("stop bootycall.service")

              # Write a custom user file in the TFTP root
              server.succeed("echo 'user-content' > /var/lib/bootycall/tftpboot/my-custom-file.txt")

              # Overwrite a default bootloader file with old/dummy content
              server.succeed("echo 'old-bootloader' > /var/lib/bootycall/tftpboot/boot/x64/ipxe.efi")

              # Fix ownership so the bootycall DynamicUser can manage them
              server.succeed("chown -R $(stat -c '%u:%g' /var/lib/bootycall) /var/lib/bootycall")

              # Start the service again
              server.systemctl("start bootycall.service")
              server.wait_for_unit("bootycall.service")

              # Verify that the bootloader file was updated/overwritten back to the system default
              bootloader_content = server.succeed("cat /var/lib/bootycall/tftpboot/boot/x64/ipxe.efi")
              if "old-bootloader" in bootloader_content:
                  raise Exception("Bootloader was not updated/overwritten on service restart")

              # Verify that the custom user file is still preserved
              custom_content = server.succeed("cat /var/lib/bootycall/tftpboot/my-custom-file.txt").strip()
              if custom_content != "user-content":
                  raise Exception(f"Custom user file was modified or deleted: {custom_content}")
            '';
          };
        }
      );

      # devShell setup for local developer environments
      devShells = forEachSystem (
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ inputs.rust-overlay.overlays.default ];
          };
        in
        {
          default = devenv.lib.mkShell {
            inherit inputs pkgs;
            modules = [
              ./devenv/default.nix
            ];
          };
        }
      );
    };
}
