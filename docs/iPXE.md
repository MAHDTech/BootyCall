# iPXE Binary Build & Automation Guide

This document describes how BootyCall builds and manages custom iPXE binaries. Build automation is fully integrated into the repository using **Nix Flakes** for both native AMD64 (x86_64) builds and cross-compiled ARM64 (aarch64) builds.

---

## 1. How it Works

iPXE builds are parameterized and packaged in the repository under [packages/ipxe/default.nix](../packages/ipxe/default.nix).

The builder:

- Downloads/resolves the iPXE source.
- Patches Makefile files to ensure compatibility with Nix/NixOS environments.
- Inject customizable compile-time macros into `src/config/general.h` (e.g., console formatting, network protocols, interactive commands).
- Embeds a parameterized boot script to chainload the main configurations.
- Compiles the target firmware binaries (e.g., `ipxe.efi` and `snponly.efi`).
- Cross-compiles using Nix cross-toolchains (`pkgsCross`) when targeting alternative CPU architectures.

---

## 2. Default Compile-Time Options

The following options are enabled by default in our automated builds (defined in [`packages/ipxe/amd64.nix`](../packages/ipxe/amd64.nix) and [`packages/ipxe/arm64.nix`](../packages/ipxe/arm64.nix)):

- `CONSOLE_CMD` - Interactive console commands, colors, and console pairing.
- `CONSOLE_FRAMEBUFFER` - Framebuffer console display.
- `IMAGE_PNG` - PNG wallpaper background support.
- `REBOOT_CMD` / `POWEROFF_CMD` - Allow machine reboot and power-off from shell/scripts.
- `NTP_CMD` - Network time synchronization.
- `NSLOOKUP_CMD` - DNS lookup diagnostics.
- `DOWNLOAD_PROTO_TFTP` - Core TFTP download support. HTTP is enabled in
  iPXE by default, so it isn't listed in `additionalConfig`; HTTPS is
  intentionally not turned on. `PING_CMD` was documented previously but
  is not currently enabled.

---

## 3. Parameterized Embed Script

The embedded bootstrap script resolves the BootyCall server configuration dynamically:

- **Dynamic DHCP Resolution**: If no hardcoded IP is provided, it tries to chainload using the DHCP-provided `next-server` variable over TFTP, standard HTTP (port 80), and then HTTP on port `8080` (BootyCall HTTP default).
- **Static Hardcoding**: Allows defining a custom server IP/hostname and custom HTTP port at build-time.

```ipxe
#!ipxe

# Ensure the interface is up and DHCP is run
ifopen || goto fail
dhcp || goto fail

echo "Booting from BootyCall server: ${server}"

# Try loading config via TFTP, then standard HTTP, then custom HTTP port
chain --autofree tftp://${server}/ipxe/config.ipxe || \
chain --autofree http://${server}/ipxe/config.ipxe || \
chain --autofree http://${server}:${portStr}/ipxe/config.ipxe || \
goto fail

:fail
echo "BootyCall boot failed. Dropping to interactive iPXE shell..."
shell
reboot
```

---

## 4. Local Development and Testing

The built binaries are ignored in git via [.gitignore](../.gitignore) so they do not pollute source control, but you can build and place them locally for testing.

### Command-Line Compilation

- **Build AMD64 UEFI binaries (`ipxe.efi` and `snponly.efi`)**:

  ```bash
  nix build .#ipxe-amd64
  ```

- **Build ARM64 UEFI binaries (`ipxe.efi` and `snponly.efi`)**:

  ```bash
  nix build .#ipxe-arm64
  ```

- **Build the full assets bundle (places binaries under `tftpboot/boot/x64` and `tftpboot/boot/arm64`)**:

  ```bash
  nix build .#assets
  ```

### Populating Workspace for Local Runs

For ease of testing, a devenv helper script is provided. Simply run:

```bash
nix develop --impure --command build-ipxe-local
```

This script will build both AMD64 and ARM64 binaries and copy them into your local [tftpboot/boot/](../tftpboot/boot/) folder structure.

---

## 5. CI/CD Release Assets

When a new version is tagged and released, the GitHub Action release pipeline:

1. Compiles `ipxe-amd64` and `ipxe-arm64` from source via Nix.
2. Automatically attaches the built binaries to the GitHub Release:
   - `ipxe-amd64.efi`
   - `snponly-amd64.efi`
   - `ipxe-arm64.efi`
   - `snponly-arm64.efi`

---

## 6. NixOS Deployment Integration

When deploying via the BootyCall NixOS service, the system automatically builds these packages from the flake and seeds them into the TFTP server's directory on startup (if `services.bootycall.seedDefaultAssets` is set to `true`).

---

## 7. Legacy Manual Reference (Archived)

For details on manual dependencies, symlinking toolchains, and manually compiling before this automation, please refer to the Git history or run commands inside a standard cross-compilation shell (`shell-arm64.nix`/`shell-amd64.nix`).
