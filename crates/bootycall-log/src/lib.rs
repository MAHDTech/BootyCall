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

#[cfg(test)]
#[derive(Clone, Debug)]
pub(crate) enum MockEnvVal {
    Present(String),
    NotPresent,
    NotUnicode,
}

#[cfg(test)]
static TEST_BOOTYCALL_LOG_FORMAT: std::sync::Mutex<Option<MockEnvVal>> =
    std::sync::Mutex::new(None);
#[cfg(test)]
static TEST_RUST_LOG: std::sync::Mutex<Option<MockEnvVal>> = std::sync::Mutex::new(None);

fn get_bootycall_log_format() -> Result<String, std::env::VarError> {
    #[cfg(test)]
    {
        let lock = TEST_BOOTYCALL_LOG_FORMAT
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(mock) = &*lock {
            return match mock {
                MockEnvVal::Present(val) => Ok(val.clone()),
                MockEnvVal::NotPresent => Err(std::env::VarError::NotPresent),
                MockEnvVal::NotUnicode => {
                    Err(std::env::VarError::NotUnicode(std::ffi::OsString::new()))
                }
            };
        }
    }
    std::env::var("BOOTYCALL_LOG_FORMAT")
}

fn get_rust_log() -> Result<String, std::env::VarError> {
    #[cfg(test)]
    {
        let lock = TEST_RUST_LOG.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(mock) = &*lock {
            return match mock {
                MockEnvVal::Present(val) => Ok(val.clone()),
                MockEnvVal::NotPresent => Err(std::env::VarError::NotPresent),
                MockEnvVal::NotUnicode => {
                    Err(std::env::VarError::NotUnicode(std::ffi::OsString::new()))
                }
            };
        }
    }
    std::env::var(EnvFilter::DEFAULT_ENV)
}

/// Parse `raw` as `RUST_LOG` directives. If the value is malformed, emit a
/// one-line warning naming the bad value and keep any remaining valid directives.
/// The warning goes to stderr via `eprintln!` because no subscriber is active yet.
fn filter_or_fallback(raw: &str) -> EnvFilter {
    match EnvFilter::try_new(raw) {
        Ok(filter) => filter,
        Err(err) => {
            eprintln!(
                "bootycall-log: malformed {}={raw:?} ({err}); keeping valid directives",
                EnvFilter::DEFAULT_ENV
            );
            EnvFilter::builder()
                .with_default_directive(tracing_subscriber::filter::LevelFilter::INFO.into())
                .parse_lossy(raw)
        }
    }
}

fn env_filter() -> EnvFilter {
    match get_rust_log() {
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

fn build_subscriber_with_writer<W>(
    writer: W,
) -> Box<dyn tracing::Subscriber + Send + Sync + 'static>
where
    W: for<'writer> tracing_subscriber::fmt::MakeWriter<'writer> + Send + Sync + 'static,
{
    let json = get_bootycall_log_format()
        .map(|v| v.eq_ignore_ascii_case("json"))
        .unwrap_or(false);

    if json {
        Box::new(
            tracing_subscriber::fmt()
                .json()
                .with_writer(writer)
                .with_env_filter(env_filter())
                .finish(),
        )
    } else {
        Box::new(
            tracing_subscriber::fmt()
                .with_writer(writer)
                .with_env_filter(env_filter())
                .finish(),
        )
    }
}

fn build_subscriber() -> Box<dyn tracing::Subscriber + Send + Sync + 'static> {
    build_subscriber_with_writer(std::io::stdout)
}

/// Initialise logging, returning an error instead of panicking when a global
/// subscriber is already installed.
///
/// `BOOTYCALL_LOG_FORMAT=json` selects one-JSON-object-per-line output (for
/// the systemd unit and ClickHouse ingestion); any other value, or unset,
/// uses the human-readable formatter. The env filter (`RUST_LOG`) applies to
/// both; a malformed `RUST_LOG` prints a one-line stderr warning and keeps
/// the remaining valid directives.
///
/// # Errors
///
/// Returns [`LogInitError::AlreadyInitialized`] if a global `tracing`
/// subscriber (or `log` logger) has already been installed.
pub fn try_init() -> Result<(), LogInitError> {
    let subscriber = build_subscriber();
    subscriber.try_init().map_err(LogInitError::from)
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

    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    struct EnvGuard<'a> {
        _lock: std::sync::MutexGuard<'a, ()>,
    }

    impl<'a> EnvGuard<'a> {
        fn new(format: Option<MockEnvVal>, rust_log: Option<MockEnvVal>) -> Self {
            let lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            *TEST_BOOTYCALL_LOG_FORMAT
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = format;
            *TEST_RUST_LOG.lock().unwrap_or_else(|e| e.into_inner()) = rust_log;
            EnvGuard { _lock: lock }
        }
    }

    impl Drop for EnvGuard<'_> {
        fn drop(&mut self) {
            *TEST_BOOTYCALL_LOG_FORMAT
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = None;
            *TEST_RUST_LOG.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }

    /// A `MakeWriter` that appends everything into a shared buffer so a test
    /// can inspect the formatted output.
    #[derive(Clone, Default)]
    struct BufWriter(Arc<Mutex<Vec<u8>>>);

    impl io::Write for BufWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend_from_slice(buf);
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

        let out = String::from_utf8(buf.lock().unwrap_or_else(|e| e.into_inner()).clone()).unwrap();
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
        let _guard = EnvGuard::new(None, Some(MockEnvVal::Present("info".to_string())));
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

    #[test]
    fn env_filter_not_unicode() {
        let _guard = EnvGuard::new(None, Some(MockEnvVal::NotUnicode));
        let filter = env_filter();
        assert_eq!(filter.to_string(), DEFAULT_DIRECTIVES);
    }

    #[test]
    fn env_filter_not_present() {
        let _guard = EnvGuard::new(None, Some(MockEnvVal::NotPresent));
        let filter = env_filter();
        assert_eq!(filter.to_string(), DEFAULT_DIRECTIVES);
    }

    #[test]
    fn format_selection_json() {
        let _guard = EnvGuard::new(
            Some(MockEnvVal::Present("json".to_string())),
            Some(MockEnvVal::Present("info".to_string())),
        );
        let buf = Arc::new(Mutex::new(Vec::new()));
        let subscriber = build_subscriber_with_writer(BufWriter(buf.clone()));

        tracing::subscriber::with_default(subscriber, || {
            info!("hello from json test");
        });

        let out = String::from_utf8(buf.lock().unwrap_or_else(|e| e.into_inner()).clone()).unwrap();
        assert!(out.contains("hello from json test"));
        let _val: serde_json::Value = serde_json::from_str(&out).expect("must be valid JSON");
    }

    #[test]
    fn format_selection_text() {
        let _guard = EnvGuard::new(
            Some(MockEnvVal::Present("text".to_string())),
            Some(MockEnvVal::Present("info".to_string())),
        );
        let buf = Arc::new(Mutex::new(Vec::new()));
        let subscriber = build_subscriber_with_writer(BufWriter(buf.clone()));

        tracing::subscriber::with_default(subscriber, || {
            info!("hello from text test");
        });

        let out = String::from_utf8(buf.lock().unwrap_or_else(|e| e.into_inner()).clone()).unwrap();
        assert!(out.contains("hello from text test"));
        assert!(!out.trim().starts_with('{'));
    }

    #[test]
    fn format_selection_unset() {
        let _guard = EnvGuard::new(
            Some(MockEnvVal::NotPresent),
            Some(MockEnvVal::Present("info".to_string())),
        );
        let buf = Arc::new(Mutex::new(Vec::new()));
        let subscriber = build_subscriber_with_writer(BufWriter(buf.clone()));

        tracing::subscriber::with_default(subscriber, || {
            info!("hello from unset test");
        });

        let out = String::from_utf8(buf.lock().unwrap_or_else(|e| e.into_inner()).clone()).unwrap();
        assert!(out.contains("hello from unset test"));
        assert!(!out.trim().starts_with('{'));
    }

    #[test]
    fn filter_or_fallback_lossy_keeps_valid_directives() {
        let filter = filter_or_fallback("warn,foo=bar=baz");
        assert_eq!(filter.to_string(), "warn");
    }

    #[test]
    fn filter_or_fallback_lossy_keeps_multiple_valid_directives() {
        let filter = filter_or_fallback("warn,bootycall::events=info,bad=directive");
        let filter_str = filter.to_string();
        assert!(filter_str.contains("warn"));
        assert!(filter_str.contains("bootycall::events=info"));
        assert!(!filter_str.contains("bad=directive"));
    }
}
