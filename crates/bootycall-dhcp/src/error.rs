use thiserror::Error;

/// Errors surfaced by the proxy DHCP server's public API.
///
/// Per AGENTS.md §2, library crates expose `thiserror`-based error types; the
/// CLI binary wraps them in `anyhow`. Per-packet faults (undecodable
/// datagrams, encode failures, send errors) are logged and skipped inside the
/// server loop, so only fatal socket setup failures reach the caller.
#[derive(Debug, Error)]
pub enum DhcpError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
