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
      packages = forEachSystem (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          bootycall = pkgs.callPackage ./packages/crate.nix { } "bootycall-rs";

          assets = pkgs.stdenv.mkDerivation {
            name = "bootycall-assets";
            src = ./.;
            installPhase = ''
              mkdir -p $out
              if [ -d "tftpboot" ]; then
                cp -r tftpboot $out/
              else
                mkdir -p $out/tftpboot
              fi

              if [ -d "static" ]; then
                cp -r static $out/
              else
                mkdir -p $out/static
              fi
            '';
          };

          default = self.packages.${system}.bootycall;
        }
      );

      nixosModules.default =
        {
          config,
          lib,
          pkgs,
          ...
        }:
        let
          cfg = config.services.bootycall;
          bootycallPkg = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
          assetsPkg = self.packages.${pkgs.stdenv.hostPlatform.system}.assets;

          # Extract port number from a "host:port" bind address string
          extractPort =
            bind:
            let
              parts = lib.splitString ":" bind;
            in
            lib.toInt (lib.last parts);

          # Generate the bootycall.yaml configuration file from Nix options
          generatedConfigFile = pkgs.writeText "bootycall.yaml" (
            builtins.toJSON {
              server = {
                http_bind = cfg.server.httpBind;
                tftp_bind = cfg.server.tftpBind;
                tftp_root = cfg.server.tftpRoot;
                proxy_dhcp_bind = cfg.server.proxyDhcpBind;
                cache_dir = cfg.server.cacheDir;
                default_bootloader_amd64 = cfg.server.defaultBootloaderAmd64;
                default_bootloader_arm64 = cfg.server.defaultBootloaderArm64;
              };
              hosts = map (
                h:
                {
                  inherit (h) mac name;
                  image_path = h.imagePath;
                }
                // lib.optionalAttrs (h.bootloader != null) { inherit (h) bootloader; }
                // lib.optionalAttrs (h.kernelPath != null) { kernel_path = h.kernelPath; }
                // lib.optionalAttrs (h.initrdPath != null) { initrd_path = h.initrdPath; }
                // lib.optionalAttrs (h.cmdline != null) { inherit (h) cmdline; }
              ) cfg.hosts;
            }
          );

          # The config file to use: either user-provided or generated from options
          configFile = if cfg.configFile != null then cfg.configFile else generatedConfigFile;

          # Host entry submodule type
          hostEntryType = lib.types.submodule {
            options = {
              mac = lib.mkOption {
                type = lib.types.str;
                description = "MAC address of the host (colon or hyphen separated).";
                example = "52:54:00:10:10:10";
              };
              name = lib.mkOption {
                type = lib.types.str;
                description = "Human-readable name for the host.";
                example = "my-server";
              };
              imagePath = lib.mkOption {
                type = lib.types.str;
                description = "Path to the ISO or disk image file.";
                example = "/var/lib/bootycall/images/nixos.iso";
              };
              bootloader = lib.mkOption {
                type = lib.types.nullOr lib.types.str;
                default = null;
                description = "Optional per-host bootloader override path.";
              };
              kernelPath = lib.mkOption {
                type = lib.types.nullOr lib.types.str;
                default = null;
                description = "Optional override for kernel path within the ISO.";
              };
              initrdPath = lib.mkOption {
                type = lib.types.nullOr lib.types.str;
                default = null;
                description = "Optional override for initrd path within the ISO.";
              };
              cmdline = lib.mkOption {
                type = lib.types.nullOr lib.types.str;
                default = null;
                description = "Custom kernel boot arguments.";
              };
            };
          };
        in
        {
          options.services.bootycall = {
            enable = lib.mkEnableOption "BootyCall unified network boot service";

            dataDir = lib.mkOption {
              type = lib.types.str;
              default = "/var/lib/bootycall";
              description = "Directory for BootyCall state and assets.";
            };

            configFile = lib.mkOption {
              type = lib.types.nullOr lib.types.path;
              default = null;
              description = ''
                Path to an externally-managed bootycall.yaml configuration file.
                When set, the declarative server and hosts options are ignored.
                When null (default), the configuration is generated from the
                server and hosts options below.
              '';
            };

            seedDefaultAssets = lib.mkOption {
              type = lib.types.bool;
              default = true;
              description = "Whether to automatically copy the default tftpboot and static assets into dataDir if they are missing.";
            };

            openFirewall = lib.mkOption {
              type = lib.types.bool;
              default = true;
              description = "Whether to automatically open firewall ports for BootyCall services.";
            };

            server = {
              httpBind = lib.mkOption {
                type = lib.types.str;
                default = "0.0.0.0:8080";
                description = "HTTP server listen address.";
              };

              tftpBind = lib.mkOption {
                type = lib.types.str;
                default = "0.0.0.0:69";
                description = "TFTP server listen address.";
              };

              tftpRoot = lib.mkOption {
                type = lib.types.str;
                default = "${cfg.dataDir}/tftpboot";
                defaultText = lib.literalExpression ''"''${cfg.dataDir}/tftpboot"'';
                description = "TFTP root directory for serving bootloader files.";
              };

              proxyDhcpBind = lib.mkOption {
                type = lib.types.str;
                default = "0.0.0.0:4011";
                description = "Proxy DHCP server listen address.";
              };

              cacheDir = lib.mkOption {
                type = lib.types.str;
                default = "${cfg.dataDir}/cache";
                defaultText = lib.literalExpression ''"''${cfg.dataDir}/cache"'';
                description = "Directory for extracted kernel/initrd cache.";
              };

              defaultBootloaderAmd64 = lib.mkOption {
                type = lib.types.str;
                default = "boot/x64/ipxe.efi";
                description = "Default bootloader path for x86_64 UEFI clients.";
              };

              defaultBootloaderArm64 = lib.mkOption {
                type = lib.types.str;
                default = "boot/arm64/ipxe.efi";
                description = "Default bootloader path for ARM64 UEFI clients.";
              };
            };

            hosts = lib.mkOption {
              type = lib.types.listOf hostEntryType;
              default = [ ];
              description = "List of host entries mapping MAC addresses to boot images.";
              example = [
                {
                  mac = "52:54:00:10:10:10";
                  name = "my-server";
                  imagePath = "/var/lib/bootycall/images/nixos.iso";
                }
              ];
            };

            firewallPorts = {
              tcp = lib.mkOption {
                type = lib.types.listOf lib.types.port;
                default = [ (extractPort cfg.server.httpBind) ];
                defaultText = lib.literalExpression "[ (extracted from server.httpBind) ]";
                description = "TCP ports to open in the firewall when openFirewall is true.";
              };

              udp = lib.mkOption {
                type = lib.types.listOf lib.types.port;
                default = [
                  (extractPort cfg.server.tftpBind)
                  (extractPort cfg.server.proxyDhcpBind)
                ];
                defaultText = lib.literalExpression "[ (extracted from server.tftpBind) (extracted from server.proxyDhcpBind) ]";
                description = "UDP ports to open in the firewall when openFirewall is true.";
              };
            };
          };

          config = lib.mkIf cfg.enable {
            networking.firewall = lib.mkIf cfg.openFirewall {
              allowedTCPPorts = cfg.firewallPorts.tcp;
              allowedUDPPorts = cfg.firewallPorts.udp;
            };

            systemd.services.bootycall = {
              description = "BootyCall Unified Network Boot Service";
              wantedBy = [ "multi-user.target" ];
              after = [ "network.target" ];

              preStart = lib.mkIf cfg.seedDefaultAssets ''
                # Create directories if they don't exist
                mkdir -p ${cfg.dataDir}/tftpboot ${cfg.dataDir}/static ${cfg.server.cacheDir}

                # Copy default TFTP assets, preserving user additions
                if [ -d "${assetsPkg}/tftpboot" ] && [ "$(ls -A ${assetsPkg}/tftpboot)" ]; then
                  cp -rn ${assetsPkg}/tftpboot/* ${cfg.dataDir}/tftpboot/ || true
                  chmod -R u+w ${cfg.dataDir}/tftpboot
                fi

                # Copy default static assets, preserving user additions
                if [ -d "${assetsPkg}/static" ] && [ "$(ls -A ${assetsPkg}/static)" ]; then
                  cp -rn ${assetsPkg}/static/* ${cfg.dataDir}/static/ || true
                  chmod -R u+w ${cfg.dataDir}/static
                fi
              '';

              serviceConfig = {
                ExecStart = "${bootycallPkg}/bin/bootycall-rs --config ${configFile}";
                Restart = "always";
                DynamicUser = true;
                StateDirectory = "bootycall";
                WorkingDirectory = cfg.dataDir;
                Environment = [
                  "TFTP_ROOT=${cfg.server.tftpRoot}"
                  "STATIC_ROOT=${cfg.dataDir}/static"
                ];
                AmbientCapabilities = [ "CAP_NET_BIND_SERVICE" ];
                CapabilityBoundingSet = [ "CAP_NET_BIND_SERVICE" ];
                NoNewPrivileges = true;
                PrivateDevices = true;
                ProtectSystem = "strict";
                ProtectHome = true;
                ReadWritePaths = [
                  cfg.dataDir
                  cfg.server.cacheDir
                ];
              };
            };
          };
        };

      devShells = forEachSystem (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
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
