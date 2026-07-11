use thiserror::Error;

/// Errors surfaced by the TFTP server.
///
/// Per AGENTS.md §2, library crates expose `thiserror`-based error types; the
/// CLI binary wraps them in `anyhow`. The public `run_tftp_server*` entry
/// points only fail on fatal socket setup errors (`Io`); the timeout variants
/// come from per-transfer tasks, whose errors are logged by the server loop
/// rather than propagated (a single stalled client must not kill the server).
#[derive(Debug, Error)]
pub enum TftpError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("OACK negotiation timed out after max retries")]
    OackNegotiationTimedOut,

    #[error("timed out waiting for a data-block ACK")]
    BlockAckTimedOut,
}
