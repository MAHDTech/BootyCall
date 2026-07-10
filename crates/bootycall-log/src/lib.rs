pub use tracing::{debug, error, info, trace, warn};
use tracing_subscriber::EnvFilter;

/// Tracing target for structured, machine-consumed lifecycle events. A log
/// collector (e.g. Vector) routes records carrying this target into
/// ClickHouse; everything else is human log output. Emit with the [`event!`]
/// macro, or directly with
/// `info!(target: bootycall_log::EVENT_TARGET, event = "...", ...)`.
pub const EVENT_TARGET: &str = "bootycall::events";

/// Emit a structured lifecycle event on [`EVENT_TARGET`] at INFO level.
///
/// The first argument is the event name; the rest are `tracing` fields:
/// `event!("tftp_transfer_complete", mac = %mac, bytes = n);`
///
/// In `BOOTYCALL_LOG_FORMAT=json` mode these serialise to one JSON object per
/// line for downstream ingestion; in text mode they render as normal logs.
#[macro_export]
macro_rules! event {
    ($name:expr $(, $($field:tt)*)?) => {
        $crate::info!(target: $crate::EVENT_TARGET, event = $name $(, $($field)*)?)
    };
}

fn env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
}

/// Initialise logging. `BOOTYCALL_LOG_FORMAT=json` selects one-JSON-object-
/// per-line output (for the systemd unit and ClickHouse ingestion); any other
/// value, or unset, uses the human-readable formatter. The env filter
/// (`RUST_LOG`) applies to both.
pub fn init() {
    let json = std::env::var("BOOTYCALL_LOG_FORMAT")
        .map(|v| v.eq_ignore_ascii_case("json"))
        .unwrap_or(false);

    if json {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(env_filter())
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(env_filter())
            .init();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::fmt::MakeWriter;

    /// A `MakeWriter` that appends everything into a shared buffer so a test
    /// can inspect the formatted output.
    #[derive(Clone, Default)]
    struct BufWriter(Arc<Mutex<Vec<u8>>>);

    impl io::Write for BufWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("buffer lock").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for BufWriter {
        type Writer = BufWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn json_event_emits_parseable_object_with_fields() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_writer(BufWriter(buf.clone()))
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            event!(
                "tftp_transfer_complete",
                mac = "aa:bb:cc:dd:ee:ff",
                bytes = 512u64
            );
        });

        let out = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        let line = out.lines().next().expect("expected one JSON line");
        let v: serde_json::Value = serde_json::from_str(line).expect("output must be valid JSON");

        assert_eq!(v["target"], EVENT_TARGET);
        assert_eq!(v["fields"]["event"], "tftp_transfer_complete");
        assert_eq!(v["fields"]["mac"], "aa:bb:cc:dd:ee:ff");
        assert_eq!(v["fields"]["bytes"], 512);
    }
}
