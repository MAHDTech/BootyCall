pub mod server;

pub use server::run_http_server;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_run_http_server_export_exists() {
        // Verify that run_http_server is re-exported and accessible.
        // A compile-time check: if this module compiles, the export exists.
        let _ = run_http_server as fn(_, _, _) -> _;
    }
}
