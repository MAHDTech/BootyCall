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
        in
        {
          options.services.bootycall = {
            enable = lib.mkEnableOption "BootyCall unified network boot service";
            dataDir = lib.mkOption {
              type = lib.types.str;
              default = "/var/lib/bootycall";
              description = "Directory for BootyCall state and assets.";
            };
            seedDefaultAssets = lib.mkOption {
              type = lib.types.bool;
              default = true;
              description = "Whether to automatically copy the default tftpboot and static assets into dataDir if they are missing.";
            };
          };

          config = lib.mkIf cfg.enable {
            networking.firewall.allowedTCPPorts = [
              80
              443
            ];
            networking.firewall.allowedUDPPorts = [ 69 ];

            systemd.services.bootycall = {
              description = "BootyCall Unified Network Boot Service";
              wantedBy = [ "multi-user.target" ];
              after = [ "network.target" ];

              preStart = lib.mkIf cfg.seedDefaultAssets ''
                # Create directories if they don't exist
                mkdir -p ${cfg.dataDir}/tftpboot ${cfg.dataDir}/static

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
                ExecStart = "${bootycallPkg}/bin/bootycall-rs";
                Restart = "always";
                DynamicUser = true;
                StateDirectory = "bootycall";
                WorkingDirectory = cfg.dataDir;
                Environment = [
                  "TFTP_ROOT=${cfg.dataDir}/tftpboot"
                  "STATIC_ROOT=${cfg.dataDir}/static"
                ];
                AmbientCapabilities = [ "CAP_NET_BIND_SERVICE" ];
                CapabilityBoundingSet = [ "CAP_NET_BIND_SERVICE" ];
                NoNewPrivileges = true;
                PrivateDevices = true;
                ProtectSystem = "strict";
                ProtectHome = true;
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
