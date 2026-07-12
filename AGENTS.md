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
- **Error Handling**: Use `thiserror` for library crates and `anyhow` or custom wrappers in the main CLI app. All errors must be propagated cleanly.
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

#### Stage changes so Nix can see them

**Nix and devenv only see files that git tracks.** `nix develop`, `devenv`, and the Nix package build (`packages/crate.nix`) evaluate from the git source, so anything you have not staged with `git add` is invisible to them:

- A **new** file (e.g. a new module) fails the build with `file not found for module ...`.
- Edits to existing files build from the last staged/committed version, not your working copy.
- This is exactly what the `warning: Git tree '...' is dirty` message is telling you.

Always `git add` your changes before running `nix develop`/`devenv` (or committing, which builds via the hooks) — stage new files in particular _before_ the first `nix develop` invocation that needs them.

This runs check-yaml, cspell, action-validator, cargo-check, clippy, rustfmt, statix, deadnix, shellcheck, markdownlint, lychee, and friends, to verify code correctness and clean style.

Run `nix develop --impure --command devenv test` before committing: it runs the
full git-hook battery **and** the workspace test suite (`cargo test --workspace
--all-features`), failing if any test fails.

### Documentation & Link Standards

- **Relative Paths Only**: Always use repository-relative URLs (e.g., `[default.nix](./packages/ipxe/default.nix)` from the repository root, or `[default.nix](../packages/ipxe/default.nix)` from files within the `docs/` subdirectory) for all documentation links referencing repository files.
- **No Absolute `file://` Links**: NEVER commit absolute `file://` links in repository markdown files (such as `docs/*.md` or `README.md`). They are not portable and will cause link-checking (`lychee`) failures in CI.

---

## 3. Crate Responsibilities

The Cargo Workspace is located inside the root project directory and splits responsibilities across the following crates inside the `crates/` folder:

- `bootycall-core`: Configuration definitions, file monitoring, event registry, and global status state; shared helpers like `normalize_mac`, `is_valid_mac`, and `safe_join`.
- `bootycall-dhcp`: Proxy DHCP server. Parses Option 60 (vendor class) and Option 93 (architecture) and serves architecture-specific PXE redirection to `PXEClient`s only.
- `bootycall-tftp`: Asynchronous TFTP server serving UEFI bootloader binaries. Concurrent transfers are bounded by a semaphore.
- `bootycall-extractor`: User-space extraction of kernels/initrds from ISOs/disk images to a persistent cache. Cache metadata is JSON.
- `bootycall-http`: HTTP endpoints for dynamic iPXE scripts, file streaming, wallpaper picker, and UI. The mutating `/api/override` endpoint is gated on an optional `api_token`.
- `bootycall-oled`: 160×60 OLED render loop, page state machine, and TrueType text rendering via `ab_glyph`. Runs on a dedicated OS thread.
- `bootycall-led`: Rackmount status LED driver (blue/white/off) and a boot-blink pattern used during startup.
- `bootycall-log`: Thin wrapper around `tracing`/`tracing-subscriber` so every crate emits structured logs the same way.
- `bootycall-rs`: Main binary orchestrator running the Tokio runtime and reading CLI flags.

---

## 4. Conventional Commits

All commits must follow the **Conventional Commits** specification:

- `feat: ...` for new features
- `fix: ...` for bugs
- `docs: ...` for documentation modifications
- `chore: ...` for environment updates, workspace settings, etc.
- `test: ...` for test suites addition
