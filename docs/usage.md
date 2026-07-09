# Development Usage Guide: bootycall-rs

This guide explains how to configure, run, and test the `bootycall-rs` suite during development.

---

## 1. Directory Structure Setup

Before running the server, ensure you have the expected configuration and assets directories:

```bash
mkdir -p static/wallpapers
mkdir -p tftpboot/boot/x64
mkdir -p tftpboot/boot/arm64
```

- **Wallpapers**: Put any `.png` or `.jpg` background images under `static/wallpapers/`.
- **Bootloaders**: Place `ipxe.efi` files under `tftpboot/boot/x64/ipxe.efi` and `tftpboot/boot/arm64/ipxe.efi`.

---

## 2. Configuration (`bootycall.yaml`)

Create a local `bootycall.yaml` in the project root:

```yaml
server:
  http_bind: "0.0.0.0:8080"
  tftp_bind: "0.0.0.0:69"
  tftp_root: "./tftpboot"
  proxy_dhcp_bind: "0.0.0.0:4011"
  cache_dir: "./cache"
  default_bootloader_amd64: "boot/x64/ipxe.efi"
  default_bootloader_arm64: "boot/arm64/ipxe.efi"

hosts:
  - mac: "52:54:00:10:10:10"
    name: "nixos-amd64-installer"
    image_path: "/var/lib/bootycall/images/nixos-minimal-23.11-x86_64-linux.iso"
```

---

## 3. Running on Privileged Ports (69/UDP & 67/UDP)

Binding to ports below 1024 on Linux requires special permissions. You can run the server in two ways:

### Option A: Use Non-Privileged Ports (Recommended for local dev)

Modify `bootycall.yaml` to use high ports:

- `tftp_bind: "0.0.0.0:6969"`
- `proxy_dhcp_bind: "0.0.0.0:4011"`

### Option B: Grant Network Capabilities to Binary

To run on standard ports (69/UDP, 67/UDP) without using `sudo` or `root`:

1. Build the binary:

   ```bash
   nix develop --impure --command cargo build
   ```

2. Grant bind capabilities:

   ```bash
   sudo setcap 'cap_net_bind_service=+ep' target/debug/bootycall-rs
   ```

3. Execute the binary:

   ```bash
   ./target/debug/bootycall-rs --config bootycall.yaml
   ```

---

## 4. Standard Commands

### Build Crate

```bash
nix develop --impure --command cargo build
```

### Run Server Loop

```bash
nix develop --impure --command cargo run -p bootycall-rs -- --config bootycall.yaml
```

### Validate Configuration

Load and validate the configuration file, then exit **without** starting any
servers or touching hardware. Exits `0` when the configuration is valid and
non-zero (printing the validation error) when it is not — ideal as a pre-deploy
gate before atomically renaming a new config into place:

```bash
nix develop --impure --command cargo run -p bootycall-rs -- --config bootycall.yaml check-config
```

### Run Tests

```bash
nix develop --impure --command cargo test --all
```

### Run Linter Checks

```bash
nix develop --impure --command pre-commit run --all-files
```
