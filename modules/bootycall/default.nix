self:
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

  # Parse the port out of a "host:port" bind address, returning null when the
  # address has no ":port" (e.g. a bare IP). Returning null rather than calling
  # lib.toInt on the whole string lets the assertions below report a clear
  # error instead of crashing deep inside the firewall port defaults.
  bindPort =
    bind:
    let
      parts = lib.splitString ":" bind;
      portStr = lib.last parts;
    in
    if lib.length parts >= 2 && portStr != "" then lib.toInt portStr else null;

  # The firewall port defaults need a concrete port; fall back to a harmless
  # placeholder for a malformed bind. The matching assertion fails the build
  # before the firewall is ever configured, so the placeholder never ships.
  extractPort =
    bind:
    let
      port = bindPort bind;
    in
    if port == null then 0 else port;

  # Bind options that must carry a ":port", checked by assertions below.
  portBinds = [
    {
      name = "server.httpBind";
      value = cfg.server.httpBind;
    }
    {
      name = "server.tftpBind";
      value = cfg.server.tftpBind;
    }
    {
      name = "server.proxyDhcpBind";
      value = cfg.server.proxyDhcpBind;
    }
  ];

  # Generate the bootycall.yaml configuration file from Nix options
  generatedConfigFile = pkgs.writeText "bootycall.yaml" (
    builtins.toJSON {
      server = {
        http_bind = cfg.server.httpBind;
        tftp_bind = cfg.server.tftpBind;
        tftp_root = cfg.server.tftpRoot;
        proxy_dhcp_bind = cfg.server.proxyDhcpBind;
        cache_dir = cfg.server.cacheDir;
        # Absolute static-asset root so serving is independent of the unit's
        # working directory (matches where preStart seeds the default assets).
        static_dir = "${cfg.dataDir}/static";
        default_bootloader_amd64 = cfg.server.defaultBootloaderAmd64;
        default_bootloader_arm64 = cfg.server.defaultBootloaderArm64;
        default_bootloader_bios = cfg.server.defaultBootloaderBios;
        oled_enabled = cfg.server.oledEnabled;
        oled_brightness = cfg.server.oledBrightness;
      }
      # The service rejects an empty api_token at startup and treats an absent
      # key as "no auth", so only emit the key when a token is configured.
      // lib.optionalAttrs (cfg.server.apiToken != null) { api_token = cfg.server.apiToken; };
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

    hardware = {
      enable = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = ''
          Relax the hardened systemd unit so the OLED display and status
          LED can drive real hardware. Enabling this replaces
          `PrivateDevices = true` with targeted `DeviceAllow` entries for
          the GPIO chip and framebuffer, and adds the `gpio` and `video`
          supplementary groups so the DynamicUser can talk to
          `/dev/gpiochip0` and `/dev/fb0`. The `gpio` group is created
          automatically (it is not a NixOS default), and — unless
          `hardware.manageUdevRules` is disabled — a udev rule assigning the
          `gpiochip*` devices to it is installed too. Leave this off on
          hardware that does not have the rackmount OLED (the default
          hardening will keep BootyCall away from `/dev` entirely).
        '';
      };
      gpioDevices = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ "/dev/gpiochip0" ];
        description = "GPIO chip character devices exposed to the unit when `hardware.enable` is true.";
      };
      framebufferDevices = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ "/dev/fb0" ];
        description = "Framebuffer character devices exposed to the unit when `hardware.enable` is true.";
      };
      manageUdevRules = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = ''
          When `hardware.enable` is set, install a udev rule assigning the GPIO
          character devices (`gpiochip*`) to the `gpio` group, so the service's
          DynamicUser (a member of that group) can actually open them. The
          framebuffer already belongs to the standard `video` group. Set this
          to false to manage the udev rule yourself. See `docs/operations.md`.
        '';
      };
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

      defaultBootloaderBios = lib.mkOption {
        type = lib.types.str;
        default = "boot/x64/undionly.kpxe";
        description = "Default bootloader path for legacy BIOS PXE clients (Option 93 architecture 0), which cannot execute an EFI image.";
      };

      oledEnabled = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = ''
          Whether the service spawns the OLED render task
          (`server.oled_enabled`). Set to false on hardware without the
          rackmount OLED panel. Note that driving the real panel additionally
          requires `hardware.enable` to relax the unit hardening.
        '';
      };

      oledBrightness = lib.mkOption {
        type = lib.types.ints.u8;
        default = 255;
        description = ''
          OLED panel brightness (`server.oled_brightness`, 0-255). Lower
          values dim the display and reduce burn-in/power.
        '';
      };

      apiToken = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = ''
          Shared secret (`server.api_token`) required via the `X-API-Token`
          header on the API endpoints. When null (default), the API endpoints
          are unauthenticated. Warning: the generated configuration file is
          written to the world-readable Nix store, so any token set here is
          visible to local users; to keep the secret out of the store, provide
          an externally-managed `configFile` instead.
        '';
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
    assertions =
      # Every bind address must carry a ":port" — otherwise extractPort (used
      # for the firewall defaults) has nothing to parse. Report it clearly at
      # eval instead of crashing inside lib.toInt.
      (map (b: {
        assertion = bindPort b.value != null;
        message = ''
          services.bootycall.${b.name}: bind address "${b.value}" is missing a
          ":port" (expected e.g. "0.0.0.0:8080").
        '';
      }) portBinds)
      ++ [
        # configFile fully replaces the generated config, so declaring hosts
        # alongside it silently drops them. Make the operator pick one source.
        {
          assertion = !(cfg.configFile != null && cfg.hosts != [ ]);
          message = ''
            services.bootycall: `configFile` is set together with declarative
            `hosts`. An external `configFile` fully replaces the generated
            configuration, so the declarative `hosts` would be silently ignored.
            Provide either an external `configFile` or the declarative
            `server`/`hosts` options, not both.
          '';
        }
        # The service refuses to start on an empty api_token (it would
        # authenticate an empty header); catch it at eval time instead.
        {
          assertion = cfg.server.apiToken != "";
          message = ''
            services.bootycall.server.apiToken is set to an empty string.
            Set it to null (default) to disable API authentication, or
            provide a real secret.
          '';
        }
      ];

    networking.firewall = lib.mkIf cfg.openFirewall {
      allowedTCPPorts = cfg.firewallPorts.tcp;
      allowedUDPPorts = cfg.firewallPorts.udp;
    };

    # Assign the GPIO character devices to the `gpio` group the service's
    # DynamicUser joins, turning the "udev rules are expected to assign the
    # device nodes" note into a shipped default. See docs/operations.md.
    services.udev.extraRules = lib.mkIf (cfg.hardware.enable && cfg.hardware.manageUdevRules) ''
      SUBSYSTEM=="gpio", KERNEL=="gpiochip[0-9]*", GROUP="gpio", MODE="0660"
    '';

    # DynamicUser joins these supplementary groups when hardware.enable is
    # set. `video` is standard on NixOS but `gpio` is not, so ensure it
    # exists — otherwise the unit fails to start. The gpiochip device is
    # assigned to this group by the udev rule below (hardware.manageUdevRules).
    users.groups = lib.mkIf cfg.hardware.enable {
      gpio = { };
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
        # Readiness probe: `GET http://<http_bind>/api/health` returns 200 when
        # every configured host has cached boot artifacts ready to serve and 503
        # (JSON `{status: "degraded", hosts_not_ready: [...]}`) otherwise. An
        # external monitor — or a future sd_notify-based `WatchdogSec` — can poll
        # it to catch silent degradation.
        DynamicUser = true;
        StateDirectory = "bootycall";
        WorkingDirectory = cfg.dataDir;
        Environment = [
          "TFTP_ROOT=${cfg.server.tftpRoot}"
          "STATIC_ROOT=${cfg.dataDir}/static"
          # Emit structured lifecycle events as one JSON object per line so a
          # collector (e.g. Vector) can route them into ClickHouse. See
          # docs/observability.md.
          "BOOTYCALL_LOG_FORMAT=json"
        ];
        AmbientCapabilities = [ "CAP_NET_BIND_SERVICE" ];
        CapabilityBoundingSet = [ "CAP_NET_BIND_SERVICE" ];
        NoNewPrivileges = true;
        # `PrivateDevices = true` gives us a private /dev with no physical
        # devices — great for the network-only path, but incompatible with
        # the OLED/LED code, which needs /dev/gpiochip0 + /dev/fb0. Gate
        # the relaxation behind `hardware.enable` so the strict default
        # stands on boxes that don't have the rackmount accessory.
        PrivateDevices = !cfg.hardware.enable;
      }
      // lib.optionalAttrs cfg.hardware.enable {
        DeviceAllow = map (d: "${d} rw") (cfg.hardware.gpioDevices ++ cfg.hardware.framebufferDevices);
        # DynamicUser doesn't inherit any group memberships by default, so
        # spell out the ones needed to talk to those char devices.
        SupplementaryGroups = [
          "gpio"
          "video"
        ];
      }
      // {
        ProtectSystem = "strict";
        ProtectHome = true;
        ReadWritePaths = [
          cfg.dataDir
          cfg.server.cacheDir
        ];
      };
    };
  };
}
