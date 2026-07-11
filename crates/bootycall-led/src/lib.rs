use bootycall_log::{info, warn};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::{Mutex, OnceLock, PoisonError};
use tokio::time::{Duration, sleep};

const LED_BLUE_PATH: &str = "/sys/class/leds/blue/brightness";
const LED_WHITE_PATH: &str = "/sys/class/leds/white/brightness";

/// Number of on/off cycles the `led-test --blinking` run performs.
const LED_TEST_BLINK_CYCLES: usize = 10;
/// Per-phase blink duration for `led-test --blinking` (on for this long, then
/// off for this long).
const LED_TEST_BLINK_MS: u64 = 500;

/// Errors returned by the public LED test entry point.
#[derive(Debug, thiserror::Error)]
pub enum LedError {
    /// The requested test colour is not one of `blue`, `white`, or `off`.
    #[error("Unknown LED color: {0}. Valid colors are blue, white, off.")]
    UnknownColor(String),
}

/// Per-path availability latch (BUG-072). The rackmount LEDs are optional
/// hardware: on a host without `/sys/class/leds/{blue,white}/brightness` the
/// 500 ms `run_led_manager` tick would otherwise re-log the open failure
/// forever (~4 error lines/sec). Mirroring the OLED `PanelSink.fb_available`
/// latch, each path logs once on the present→absent transition, stays silent
/// while absent, and logs once more if the node comes back.
fn led_availability() -> &'static Mutex<HashMap<String, bool>> {
    static LED_AVAILABILITY: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
    LED_AVAILABILITY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Feed a write outcome into the availability latch, logging only on
/// available↔absent transitions for `path`.
fn note_led_write_result(path: &str, result: &std::io::Result<()>) {
    let mut availability = led_availability()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let available = availability.entry(path.to_string()).or_insert(true);
    match result {
        Ok(()) => {
            if !*available {
                info!("LED path {} is now available", path);
                *available = true;
            }
        }
        Err(e) => {
            if *available {
                warn!(
                    "LED path {} not available (optional hardware): {}; suppressing further messages until it returns",
                    path, e
                );
                *available = false;
            }
        }
    }
}

/// Raw sysfs write. Deliberately log-free: every caller routes the outcome
/// through the availability latch (`note_led_write_result`) so absent
/// hardware is logged once per transition, not per attempt.
fn write_led_sysfs(path: &str, value: u8) -> std::io::Result<()> {
    let mut file = OpenOptions::new().write(true).open(path)?;
    write!(file, "{}", value)
}

fn set_led(path: &str, value: u8) -> std::io::Result<()> {
    set_led_with(path, value, write_led_sysfs)
}

/// Same as `set_led` but takes the "actually write" closure as a parameter so
/// unit tests can exercise the availability latch without touching
/// `/sys/class/leds/*`.
fn set_led_with<F>(path: &str, value: u8, mut writer: F) -> std::io::Result<()>
where
    F: FnMut(&str, u8) -> std::io::Result<()>,
{
    let result = writer(path, value);
    note_led_write_result(path, &result);
    result
}

/// Write `value` to `path` only when it differs from the last-known-good
/// value. BUG-14: the cache is only updated on a successful write —
/// previously a failed write still poisoned `last_value`, so subsequent
/// calls thought the hardware was already in the right state and never
/// retried.
fn set_led_cached(path: &str, value: u8, last_value: &mut Option<u8>) {
    set_led_cached_with(path, value, last_value, write_led_sysfs);
}

/// Same as `set_led_cached` but takes the "actually write" closure as a
/// parameter so unit tests can exercise the caching-on-success logic
/// without touching `/sys/class/leds/*`. Writes route through
/// `set_led_with`, so failures also drive the availability latch.
fn set_led_cached_with<F>(path: &str, value: u8, last_value: &mut Option<u8>, writer: F)
where
    F: FnMut(&str, u8) -> std::io::Result<()>,
{
    if Some(value) == *last_value {
        return;
    }
    if set_led_with(path, value, writer).is_ok() {
        *last_value = Some(value);
    }
}

pub fn activate_blue_led() {
    let _ = set_led(LED_BLUE_PATH, 255);
    let _ = set_led(LED_WHITE_PATH, 0);
    info!("LED set to solid Blue (Service Running)");
}

pub fn activate_white_led() {
    let _ = set_led(LED_WHITE_PATH, 255);
    let _ = set_led(LED_BLUE_PATH, 0);
    info!("LED set to solid White (Service Stopped)");
}

/// Drive the boot-blink pattern until the stop channel resolves.
///
/// Returns `true` when a stop signal (`Some(())`) was received — the clean
/// "startup completed" stop — and `false` when the sender was dropped
/// without one (`None`), which means `main` returned early on a startup
/// failure (BUG-075).
async fn blink_until_stopped(stop_rx: &mut tokio::sync::mpsc::Receiver<()>) -> bool {
    let mut state = false;
    loop {
        tokio::select! {
            msg = stop_rx.recv() => {
                return msg.is_some();
            }
            _ = sleep(Duration::from_millis(500)) => {
                if state {
                    let _ = set_led(LED_WHITE_PATH, 255);
                    let _ = set_led(LED_BLUE_PATH, 0);
                } else {
                    let _ = set_led(LED_WHITE_PATH, 0);
                    let _ = set_led(LED_BLUE_PATH, 0);
                }
                state = !state;
            }
        }
    }
}

/// Blinks the white LED to indicate booting/initialization.
/// The `stop_rx` channel should be sent a message when booting is complete.
#[tracing::instrument(skip(stop_rx))]
pub async fn run_boot_blink(mut stop_rx: tokio::sync::mpsc::Receiver<()>) {
    if blink_until_stopped(&mut stop_rx).await {
        // Clean stop: startup completed, turn solid blue to indicate ready.
        activate_blue_led();
    } else {
        // Sender dropped without a stop signal: startup aborted before it
        // could signal completion (BUG-075). Show solid white ("Service
        // Stopped") so the rack LED never reports a healthy box that failed
        // to start. This also closes the mid-blink race: any blink tick that
        // slips in after `main`'s synchronous white write is followed by
        // this white write, never a blue one.
        activate_white_led();
    }
}

/// Background LED manager that polls StateStore for active deployments
/// and blinks Blue if active, or stays solid Blue if idle.
#[tracing::instrument(skip(state_store, shutdown_rx))]
pub async fn run_led_manager(
    state_store: bootycall_core::state::StateStore,
    mut shutdown_rx: tokio::sync::mpsc::Receiver<()>,
) {
    let mut state = false;
    let mut was_active = false;
    let mut last_blue = None;
    let mut last_white = None;
    info!("Starting LED Manager Task...");

    loop {
        // Check for active/recent deployments
        let active = state_store.has_recent_activity(Duration::from_secs(30));

        if active != was_active {
            if active {
                info!("Active deployment detected. LED set to blinking Blue.");
            } else {
                info!("System idle. LED set to solid Blue.");
            }
            was_active = active;
        }

        tokio::select! {
            _ = shutdown_rx.recv() => {
                break;
            }
            _ = sleep(Duration::from_millis(500)) => {
                if active {
                    if state {
                        set_led_cached(LED_BLUE_PATH, 255, &mut last_blue);
                        set_led_cached(LED_WHITE_PATH, 0, &mut last_white);
                    } else {
                        set_led_cached(LED_BLUE_PATH, 0, &mut last_blue);
                        set_led_cached(LED_WHITE_PATH, 0, &mut last_white);
                    }
                    state = !state;
                } else {
                    set_led_cached(LED_BLUE_PATH, 255, &mut last_blue);
                    set_led_cached(LED_WHITE_PATH, 0, &mut last_white);
                    state = true;
                }
            }
        }
    }

    // Set to solid white on clean shutdown
    set_led_cached(LED_WHITE_PATH, 255, &mut last_white);
    set_led_cached(LED_BLUE_PATH, 0, &mut last_blue);
    info!("LED set to solid White (Service Stopped)");
}

/// Dynamic LED testing for testing color and blinking states.
pub fn led_test(color: &str, blinking: bool) -> Result<(), LedError> {
    // Resolve which sysfs path is driven "on" (255) for the requested colour;
    // the paired path is always driven to 0. `off` drives both to 0 (no "on"
    // path). This collapses the previously duplicated blue/white arms — which
    // differed only in which path got 255 — into one solid/blink code path.
    let (on_path, off_path): (Option<&str>, &str) = match color {
        "blue" => (Some(LED_BLUE_PATH), LED_WHITE_PATH),
        "white" => (Some(LED_WHITE_PATH), LED_BLUE_PATH),
        "off" => (None, LED_WHITE_PATH),
        _ => {
            return Err(LedError::UnknownColor(color.to_string()));
        }
    };

    match on_path {
        None => {
            info!("Testing LEDs Off");
            let _ = set_led(LED_BLUE_PATH, 0);
            let _ = set_led(LED_WHITE_PATH, 0);
        }
        Some(on) if blinking => {
            info!("Testing Blinking {} LED for 10 seconds...", color);
            for _ in 0..LED_TEST_BLINK_CYCLES {
                let _ = set_led(on, 255);
                let _ = set_led(off_path, 0);
                std::thread::sleep(std::time::Duration::from_millis(LED_TEST_BLINK_MS));
                let _ = set_led(on, 0);
                let _ = set_led(off_path, 0);
                std::thread::sleep(std::time::Duration::from_millis(LED_TEST_BLINK_MS));
            }
        }
        Some(on) => {
            info!("Testing Solid {} LED", color);
            let _ = set_led(on, 255);
            let _ = set_led(off_path, 0);
        }
    }

    info!("LED test complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::sync::Arc;
    use tracing_subscriber::fmt::MakeWriter;
    use tracing_subscriber::util::SubscriberInitExt;

    // NOTE: the availability latch is process-global, so every test uses a
    // unique mock path name to avoid interference between parallel tests.

    /// A `MakeWriter` that appends everything into a shared buffer so a test
    /// can inspect the formatted log output.
    #[derive(Clone, Default)]
    struct BufWriter(Arc<Mutex<Vec<u8>>>);

    impl io::Write for BufWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
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

    /// Run `f` with a thread-scoped subscriber and return everything it
    /// logged as a string.
    fn capture_logs(f: impl FnOnce()) -> String {
        let buf = BufWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(buf.clone())
            .finish();
        let guard = subscriber.set_default();
        f();
        drop(guard);
        let bytes = buf.0.lock().unwrap_or_else(PoisonError::into_inner);
        String::from_utf8(bytes.clone()).expect("log output must be valid UTF-8")
    }

    fn absent() -> io::Error {
        io::Error::from(io::ErrorKind::NotFound)
    }

    #[test]
    fn cache_updates_on_successful_write() {
        let mut last = None;
        let mut writes = Vec::new();
        set_led_cached_with("test/led-cache-ok", 255, &mut last, |_, v| {
            writes.push(v);
            Ok(())
        });
        assert_eq!(last, Some(255));
        assert_eq!(writes, vec![255]);
    }

    #[test]
    fn cache_stays_none_when_write_fails() {
        let mut last = None;
        let mut writes = Vec::new();
        set_led_cached_with("test/led-cache-fail", 255, &mut last, |_, v| {
            writes.push(v);
            Err(io::Error::other("boom"))
        });
        assert_eq!(
            last, None,
            "cache must not remember a value the hardware never accepted"
        );
        assert_eq!(writes, vec![255]);
    }

    #[test]
    fn cache_hit_skips_write_entirely() {
        let mut last = Some(255);
        let mut writes = Vec::new();
        set_led_cached_with("test/led-cache-hit", 255, &mut last, |_, v| {
            writes.push(v);
            Ok(())
        });
        assert!(
            writes.is_empty(),
            "identical value should not touch the hardware"
        );
    }

    #[test]
    fn failed_write_still_retries_on_next_call() {
        // The regression BUG-14 covers: a failed first write must not
        // poison `last_value`, so the next call still attempts the write.
        let mut last = None;
        let mut attempts = 0;
        set_led_cached_with("test/led-cache-retry", 255, &mut last, |_, _| {
            attempts += 1;
            Err(io::Error::other("boom"))
        });
        set_led_cached_with("test/led-cache-retry", 255, &mut last, |_, _| {
            attempts += 1;
            Ok(())
        });
        assert_eq!(attempts, 2, "second call must retry after a failed write");
        assert_eq!(last, Some(255));
    }

    #[test]
    fn repeated_failed_cached_writes_log_absence_once() {
        // The regression BUG-072 covers: with the LED sysfs nodes absent the
        // 500 ms manager tick keeps retrying, but the absence must be logged
        // once, not on every attempt.
        let path = "test/led-absent-spam";
        let mut attempts = 0;
        let output = capture_logs(|| {
            let mut last = None;
            for _ in 0..5 {
                set_led_cached_with(path, 255, &mut last, |_, _| {
                    attempts += 1;
                    Err(absent())
                });
            }
        });
        assert_eq!(attempts, 5, "every tick must still retry the hardware");
        let absence_msg = format!("LED path {} not available", path);
        assert_eq!(
            output.matches(&absence_msg).count(),
            1,
            "absence must be logged exactly once, got:\n{output}"
        );
    }

    #[test]
    fn repeated_failed_raw_writes_log_absence_once() {
        // `run_boot_blink`, `activate_*_led`, and `led_test` drive the LEDs
        // through the raw `set_led` path; it shares the same latch, so a
        // boot-blink against absent hardware must not spam either.
        let path = "test/led-raw-absent";
        let output = capture_logs(|| {
            for _ in 0..5 {
                let _ = set_led_with(path, 255, |_, _| Err(absent()));
            }
        });
        let absence_msg = format!("LED path {} not available", path);
        assert_eq!(
            output.matches(&absence_msg).count(),
            1,
            "absence must be logged exactly once, got:\n{output}"
        );
    }

    #[test]
    fn each_availability_transition_logs_once() {
        let path = "test/led-recovery";
        let output = capture_logs(|| {
            let mut last = None;
            // absent → logs once
            set_led_cached_with(path, 255, &mut last, |_, _| Err(absent()));
            set_led_cached_with(path, 255, &mut last, |_, _| Err(absent()));
            // recovers → logs once
            set_led_cached_with(path, 255, &mut last, |_, _| Ok(()));
            // absent again (new value defeats the value cache) → logs once
            set_led_cached_with(path, 0, &mut last, |_, _| Err(absent()));
        });
        let absence_msg = format!("LED path {} not available", path);
        let recovery_msg = format!("LED path {} is now available", path);
        assert_eq!(
            output.matches(&absence_msg).count(),
            2,
            "each present→absent transition must log once, got:\n{output}"
        );
        assert_eq!(
            output.matches(&recovery_msg).count(),
            1,
            "recovery must log once, got:\n{output}"
        );
    }

    #[test]
    fn led_test_rejects_unknown_color() {
        let err = led_test("purple", false).expect_err("purple is not a valid LED color");
        assert!(matches!(err, LedError::UnknownColor(ref c) if c == "purple"));
    }

    // Both boot-blink stop tests resolve the channel before the first 500 ms
    // blink tick can fire, so `blink_until_stopped` returns without touching
    // the sysfs LED paths.

    #[tokio::test]
    async fn boot_blink_clean_stop_signal_reports_running() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        tx.send(())
            .await
            .expect("stop channel must accept a signal");
        assert!(
            blink_until_stopped(&mut rx).await,
            "an explicit stop signal is a clean stop and must resolve to the running (blue) state"
        );
    }

    #[tokio::test]
    async fn boot_blink_dropped_sender_reports_stopped() {
        // The regression BUG-075 covers: `main` returning early on a startup
        // failure drops the sender without a stop signal — that must NOT be
        // treated as a clean stop, or the rack LED turns blue ("Service
        // Running") on a box that failed to start.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(1);
        drop(tx);
        assert!(
            !blink_until_stopped(&mut rx).await,
            "a dropped sender means startup aborted and must resolve to the stopped (white) state"
        );
    }
}
