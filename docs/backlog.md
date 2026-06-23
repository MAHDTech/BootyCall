# Development Backlog: bootycall-rs

This backlog outlines the tasks required to build, test, and release the new `bootycall-rs` network booting server.

## Development Guidelines

- **Unit Testing**: Write robust unit tests for every phase of the project to guarantee correct protocol parser and file mapping operations.
- **Pre-commit Quality**: Ensure all pre-commit validation checks pass cleanly (including `cargo check`, `cargo clippy`, `rustfmt`, and all unit tests) before any commit.
- **Conventional Commits**: Commit at the end of each phase using the Conventional Commits specification (e.g. `feat: ...`, `fix: ...`, `chore: ...`).

---

## Phase 1: Workspace & Tooling Initialisation (Current)

- [x] Archive legacy code to `scratch/archived/`.
- [x] Write project specification (`docs/spec.md`).
- [x] Configure `rust-toolchain.toml` and update `devenv.nix`.
- [x] Set up pre-commit hooks for Rust formatting, syntax checks, and clippy.
- [x] Initialize Cargo Workspace structure.
- [x] Run `pre-commit` check successfully.
- [x] Perform base conventional commit (`chore: initial setup`).

---

## Phase 2: `bootycall-core` (Configuration & State Engine)

- [x] **Task 2.1**: Define YAML configuration struct schema in `config.rs`.
- [x] **Task 2.2**: Integrate `serde_yaml` to parse configuration and add unit tests.
- [x] **Task 2.3**: Implement configuration live-reloader using `notify` crate to reload on yaml changes.
- [x] **Task 2.4**: Create shared `State` structs to track currently active/polling hosts and historic boot events with thread-safe access (`Arc<RwLock>`).

---

## Phase 3: `bootycall-extractor` (ISO & Image Filesystem Parser)

- [x] **Task 3.1**: Create ISO9660 parser using `iso9660` or similar crate to traverse directories.
- [x] **Task 3.2**: Create GPT / FAT partition parser using `gpt` and `fatfs` crates to extract files from disk images.
- [x] **Task 3.3**: Implement hybrid lookup system: scans the image directory for kernel/initrd names (case-insensitive heuristics) with user overrides in YAML.
- [x] **Task 3.4**: Build the caching engine: extracts kernel and initrd to `cache/<mac>/` if modified time or file hash changed, optimising startup speed.

---

## Phase 4: `bootycall-dhcp` (Proxy DHCP Server)

- [x] **Task 4.1**: Set up UDP listener socket binding to port 4011/UDP (and port 67 if specified).
- [x] **Task 4.2**: Parse incoming DHCP Discover / Request packets using `dhcproto`.
- [x] **Task 4.3**: Implement architecture identification logic looking at Option 93 (x86_64 vs. ARM64 UEFI).
- [x] **Task 4.4**: Craft and reply with a DHCP ACK containing Option 60 (`PXEClient`), Option 66 (our IP), and Option 67 (pointing to the correct bootloader path).

---

## Phase 5: `bootycall-tftp` (Asynchronous TFTP Server)

- [x] **Task 5.1**: Set up UDP listener socket on Port 69/UDP.
- [x] **Task 5.2**: Parse Read Request (RRQ) packets and handle option negotiation (`blksize`, `timeout`).
- [x] **Task 5.3**: Build packet transmitter sending 512-byte (or negotiated `blksize`) data blocks with block counters, retries, and timeout handling.
- [x] **Task 5.4**: Integrate MAC-based bootloader path lookup to serve customized loader binaries.

---

## Phase 6: `bootycall-http` (Axum Web Server & Dynamic Scripting)

- [x] **Task 6.1**: Configure Axum HTTP routing server.
- [x] **Task 6.2**: Implement `/start` and `/poll/{mac}` endpoints. If host is mapped in state, serve custom iPXE chainload commands; otherwise return the poll retry script.
- [x] **Task 6.3**: Implement static file server endpoint serving extracted kernels and initrd files from cache.
- [x] **Task 6.4**: Implement dynamic wallpaper endpoint `/dynamic/wallpaper.ipxe` which reads files in `static/wallpapers/` and picks one at random, prioritising resolution parameters if present.
- [x] **Task 6.5**: Create iPXE menu template fallback `/ipxemenu` using `minijinja` for hosts without MAC mapping.

---

## Phase 7: Embedded UI (Glassmorphic Web Dashboard)

- [x] **Task 7.1**: Create dynamic web dashboard using vanilla HTML, modern CSS, and JS (glassmorphic dark mode).
- [x] **Task 7.2**: Bundle assets into the Rust executable using `rust-embed`.
- [x] **Task 7.3**: Expose web routes for `/api/status` (live host metrics), `/api/logs` (events), and `/api/override` (to manually select a boot script for a polling host).

---

## Phase 8: System Integration, CLI & Packaging

- [x] **Task 8.1**: Implement `main.rs` CLI parser utilising `clap`.
- [x] **Task 8.2**: Orchestrate Tokio spawn loops running DHCP, TFTP, and HTTP services concurrently.
- [x] **Task 8.3**: Integrate standard Unix signal handling (`SIGINT`/`SIGTERM`) for clean shutdowns.
- [x] **Task 8.4**: Add systemd socket activation support for port binding.
- [x] **Task 8.5**: Validate the build on both ARM64 and x86_64 target platforms.
