pub use tracing::{debug, error, info, trace, warn};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::util::SubscriberInitExt;

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

/// Filter directives used when `RUST_LOG` is unset or malformed.
const DEFAULT_DIRECTIVES: &str = "info";

/// Error returned by [`try_init`] when logging cannot be initialised.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LogInitError {
    /// A global `tracing` subscriber (or `log` logger) is already installed,
    /// for example because logging was initialised earlier in the process.
    #[error("logging already initialised: {0}")]
    AlreadyInitialized(#[from] tracing_subscriber::util::TryInitError),
}

/// Parse `raw` as `RUST_LOG` directives. If the value is malformed, emit a
/// one-line warning naming the bad value and fall back to
/// [`DEFAULT_DIRECTIVES`]. The warning goes to stderr via `eprintln!` because
/// no subscriber is active yet at this point.
fn filter_or_fallback(raw: &str) -> EnvFilter {
    EnvFilter::try_new(raw).unwrap_or_else(|err| {
        eprintln!(
            "bootycall-log: malformed {}={raw:?} ({err}); falling back to {DEFAULT_DIRECTIVES:?}",
            EnvFilter::DEFAULT_ENV
        );
        EnvFilter::new(DEFAULT_DIRECTIVES)
    })
}

fn env_filter() -> EnvFilter {
    match std::env::var(EnvFilter::DEFAULT_ENV) {
        Ok(raw) => filter_or_fallback(&raw),
        Err(std::env::VarError::NotUnicode(_)) => {
            eprintln!(
                "bootycall-log: {} is not valid UTF-8; falling back to {DEFAULT_DIRECTIVES:?}",
                EnvFilter::DEFAULT_ENV
            );
            EnvFilter::new(DEFAULT_DIRECTIVES)
        }
        Err(std::env::VarError::NotPresent) => EnvFilter::new(DEFAULT_DIRECTIVES),
    }
}

/// Initialise logging, returning an error instead of panicking when a global
/// subscriber is already installed.
///
/// `BOOTYCALL_LOG_FORMAT=json` selects one-JSON-object-per-line output (for
/// the systemd unit and ClickHouse ingestion); any other value, or unset,
/// uses the human-readable formatter. The env filter (`RUST_LOG`) applies to
/// both; a malformed `RUST_LOG` prints a one-line stderr warning and falls
/// back to `info`.
///
/// # Errors
///
/// Returns [`LogInitError::AlreadyInitialized`] if a global `tracing`
/// subscriber (or `log` logger) has already been installed.
pub fn try_init() -> Result<(), LogInitError> {
    let json = std::env::var("BOOTYCALL_LOG_FORMAT")
        .map(|v| v.eq_ignore_ascii_case("json"))
        .unwrap_or(false);

    let result = if json {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(env_filter())
            .finish()
            .try_init()
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(env_filter())
            .finish()
            .try_init()
    };
    result.map_err(LogInitError::from)
}

/// Initialise logging. Idempotent: if logging is already initialised the
/// existing subscriber is kept and this call is a no-op instead of panicking.
/// Callers that need to observe the failure should use [`try_init`].
///
/// See [`try_init`] for the `BOOTYCALL_LOG_FORMAT` and `RUST_LOG` behaviour.
pub fn init() {
    if let Err(err) = try_init() {
        // A subscriber is already installed and stays in effect; this record
        // is routed to it (and dropped under the default `info` filter).
        debug!(error = %err, "bootycall_log::init() called again; keeping existing subscriber");
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

    /// A second `init()` must be a no-op rather than a panic, and `try_init`
    /// must surface the failure as `LogInitError::AlreadyInitialized`. Both
    /// re-init paths live in one test because they share the process-global
    /// subscriber slot and would otherwise race under parallel test runs.
    #[test]
    fn init_is_idempotent_and_try_init_reports_already_initialised() {
        init();
        init(); // Second call must not panic.
        let err = try_init().expect_err("a global subscriber is already installed");
        assert!(matches!(err, LogInitError::AlreadyInitialized(_)));
    }

    #[test]
    fn malformed_directives_fall_back_to_default() {
        // `foo=bar=baz` is not a valid `RUST_LOG` directive.
        assert!(EnvFilter::try_new("foo=bar=baz").is_err());
        let filter = filter_or_fallback("foo=bar=baz");
        assert_eq!(filter.to_string(), DEFAULT_DIRECTIVES);
    }

    #[test]
    fn valid_directives_are_used_as_given() {
        let filter = filter_or_fallback("warn");
        assert_eq!(filter.to_string(), "warn");
    }
}
