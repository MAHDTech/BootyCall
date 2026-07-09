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
  # Root for static HTTP assets (wallpapers, UI files). Optional; defaults to
  # "./static". Prefer an absolute path in production so serving does not depend
  # on the process working directory.
  static_dir: "./static"
  default_bootloader_amd64: "boot/x64/ipxe.efi"
  default_bootloader_arm64: "boot/arm64/ipxe.efi"

hosts:
  - mac: "52:54:00:10:10:10"
    name: "nixos-amd64-installer"
    image_path: "/var/lib/bootycall/images/nixos-minimal-23.11-x86_64-linux.iso"
```

### Optional API token

Set `server.api_token` to require an `X-API-Token` header on the API endpoints:

```yaml
server:
  api_token: "a-long-random-secret"
```

When set, it gates the mutating `POST /api/override` **and** the read endpoints
`GET /api/status` and `GET /api/logs` (which expose host MACs, client IPs, and
log history) — requests without a matching token get `401`. When unset, all API
endpoints are open (backwards-compatible). The bundled dashboard sends the token
automatically when present in the browser's `localStorage` under the key
`bootycall_api_token` (set it once via the browser console:
`localStorage.setItem("bootycall_api_token", "a-long-random-secret")`).

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

### Health / readiness probe

`GET /api/health` is an unauthenticated readiness endpoint. It returns `200`
with `{"status":"healthy", ...}` when every configured host has non-empty cached
`kernel` + `initrd` artifacts ready to serve, and `503` with
`{"status":"degraded","hosts_not_ready":[...]}` otherwise. Poll it from a load
balancer or systemd watchdog to catch silent degradation:

```bash
curl -fsS http://localhost:8080/api/health
```

### Run Tests

```bash
nix develop --impure --command cargo test --all
```

### Run Linter Checks

```bash
nix develop --impure --command pre-commit run --all-files
```
