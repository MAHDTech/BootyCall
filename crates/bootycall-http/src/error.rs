use thiserror::Error;

/// Errors surfaced by the HTTP server's public API.
///
/// Per AGENTS.md §2, library crates expose `thiserror`-based error types; the
/// CLI binary wraps them in `anyhow`. Per-request failures are expressed as
/// HTTP status codes inside the Axum handlers, so only startup failures —
/// template registration and listener bind/serve errors — reach the caller.
#[derive(Debug, Error)]
pub enum HttpError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("failed to register {name} template: {source}")]
    TemplateRegistration {
        /// The minijinja template name (e.g. `ipxemenu`, `boot`).
        name: &'static str,
        #[source]
        source: minijinja::Error,
    },
}
