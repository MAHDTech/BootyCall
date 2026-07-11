# 📞 BootyCall

![status](https://img.shields.io/badge/status-alpha-orange?style=for-the-badge)

`bootycall-rs` is a high-performance, single-binary, all-in-one network booting suite.

It serves as a modern replacement for the legacy setup of Caddy, TFTPd, and Shoelaces.

It hosts three core protocols (Proxy DHCP, TFTP, and HTTP) on a single asynchronous Tokio runtime, working alongside your existing primary DHCP server.

It was originally written to run on a _UniFi CloudKey Gen2 Plus_ so is built to be as lightweight as possible with a single goal, to _always answer the call_ to boot your servers.

![teaser](./docs/images/teaser.png)

---

## 🚀 Key Features

- **Proxy DHCP**: Intercepts client boot requests, parses client UEFI architecture (Option 93), and redirects them to the bootloader.
- **Asynchronous TFTP**: Serves customized or default bootloaders (`ipxe.efi`) with built-in block negotiation (`blksize`, `timeout`, `tsize`) and path traversal protection.
- **ISO & Disk Extraction**: Pure Rust extraction cache for kernels and ramdisks from ISOs or disk images (GPT/FAT), lowering idle memory footprint.
- **Dynamic iPXE Scripting**: Generates customized boot scripts per host MAC address, allowing infinite polling loops until assigned.
- **Dynamic Wallpapers**: Renders randomly selected wallpaper console backgrounds on client boot, supporting resolution matching.
- **Embedded UI**: Glassmorphic dark-mode web dashboard for real-time tracking of active host boots, events logging, and manual target assignments.

---

## 🏛️ Architecture & Flow

```mermaid
sequenceDiagram
    autonumber
    actor Client as UEFI PXE Client
    participant UDM as Primary DHCP (UDM)
    participant BC_DHCP as Proxy DHCP (bootycall-rs:4011)
    participant BC_TFTP as TFTP Server (bootycall-rs:69)
    participant BC_HTTP as HTTP Server (bootycall-rs:8080)

    Client->>UDM: DHCP Discover (includes Option 93: Arch)
    UDM-->>Client: DHCP Offer (IP Lease, Gateway, DNS)
    Client->>BC_DHCP: DHCP Request/Inform (checks architecture)
    BC_DHCP-->>Client: DHCP ACK (Option 60: "PXEClient", Option 66: Next-Server, Option 67: bootloader path)
    Client->>BC_TFTP: TFTP Read Request (RRQ) for bootloader
    BC_TFTP-->>Client: Send ipxe.efi (x86_64 or arm64)
    Note over Client: Client executes ipxe.efi with embedded auto-start script
    Client->>BC_HTTP: HTTP GET /start
    BC_HTTP-->>Client: Dynamic iPXE script (checks MAC mapping)
    alt MAC mapped to ISO/IMG
        BC_HTTP-->>Client: Chain load extracted kernel & initrd from disk cache
        Client->>BC_HTTP: Fetch kernel & initrd via HTTP
        Client->>Client: Boot Target OS (e.g. NixOS Installer)
    else MAC not mapped
        BC_HTTP-->>Client: Render interactive boot menu
    end
```

---

## 🛠️ Quick Start

### 1. Build and Run via Devenv

We use [devenv](https://devenv.sh) integrated with Nix flakes for our development workspace:

```bash
nix develop --impure

cargo run -p bootycall-rs -- --config bootycall.yaml
```

_The binary is automatically compiled and added to your shell path when entering the shell._

### 2. Run Tests

To run the complete workspace integration tests:

```bash
cargo test --all
```

---

## 📖 Further Documentation

- [Specification Guide](docs/spec.md) - Deep architectural and schema details.
- [Development Usage Guide](docs/usage.md) - How to setup directory trees, configure yaml, and bind privileged ports.
- [Operations Runbook](docs/operations.md) - Day-two operations: cache reset, GPIO/udev requirements, unit recovery, and logs.
- [Observability Guide](docs/observability.md) - Structured event schema and shipping boot activity into ClickHouse.
