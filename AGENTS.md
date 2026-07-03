# AGENTS.md for bootycall-rs

**Single source of truth for all AI agents working on bootycall-rs.**

Welcome! This document outlines code standards, architectural constraints, and development processes for constructing `bootycall-rs` (the Rust-based UEFI iPXE booting server suite).

---

## 1. Project Specifications & Rules

- **Specifications**: See [spec](./docs/spec.md) for architecture, configuration formats, and dynamic features.
- **Style and Integrity**: Maintain all documentation, comments, and docstrings. Write idiomatic, readable, and highly performant Rust code with minimum dependencies.

---

## 2. Coding Standards & Tooling

### Rust Design Rules

- **Safety**: Do not use `unsafe` code blocks. All system integrations must rely on safe Rust wrappers.
- **Error Handling**: Use `thiserror` for library crates and `anyhow` or custom wrappers in the main CLI app. All errors must be propagates cleanly.
- **Asynchronous Runtime**: Standardise on `tokio` for UDP (DHCP/TFTP) sockets and TCP (HTTP) listeners.
- **Logging/Observability**: Use the `tracing` library. Instrument important server loops, packet parsed functions, and file handlers with `tracing::instrument`.
- **Formatting**: Code formatting is enforced via `rustfmt`. Clippy warnings are treated as compilation errors (`-D warnings`).

### Pre-commit Validation

Before committing code, make sure to execute:

```bash
nix develop --impure --command prek run --all-files
```

> [!IMPORTANT]
> To run commands inside the development shell non-interactively, always prefix them with `nix develop --impure --command`, for example:
> `nix develop --impure --command cargo init` or `nix develop --impure --command cargo check`.

This runs check-yaml, typos, action-validator, cargo-check, clippy, rustfmt, etc., to verify code correctness and clean style.

---

## 3. Crate Responsibilities

The Cargo Workspace is located inside the root project directory and splits responsibilities across the following crates inside the `crates/` folder:

- `bootycall-core`: Configuration definitions, file monitoring, event registry, and global status state.
- `bootycall-dhcp`: Proxy DHCP server parsing Option 93 and serving architecture-specific PXE redirection.
- `bootycall-tftp`: Asynchronous TFTP server serving UEFI bootloader binaries.
- `bootycall-extractor`: User-space extraction of kernels/initrds from ISOs/disk images to a persistent cache.
- `bootycall-http`: HTTP endpoints for dynamic iPXE scripts, file streaming, wallpaper picker, and UI.
- `bootycall-rs`: Main binary orchestrator running the Tokio runtime and reading CLI flags.

---

## 4. Conventional Commits

All commits must follow the **Conventional Commits** specification:

- `feat: ...` for new features
- `fix: ...` for bugs
- `docs: ...` for documentation modifications
- `chore: ...` for environment updates, workspace settings, etc.
- `test: ...` for test suites addition
