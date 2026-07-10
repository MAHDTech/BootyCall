use bootycall_log::info;
use std::fs::OpenOptions;
use std::io::Write;
use tokio::time::{Duration, sleep};

const LED_BLUE_PATH: &str = "/sys/class/leds/blue/brightness";
const LED_WHITE_PATH: &str = "/sys/class/leds/white/brightness";

/// Number of on/off cycles the `led-test --blinking` run performs.
const LED_TEST_BLINK_CYCLES: usize = 10;
/// Per-phase blink duration for `led-test --blinking` (on for this long, then
/// off for this long).
const LED_TEST_BLINK_MS: u64 = 500;

fn set_led(path: &str, value: u8) -> std::io::Result<()> {
    let mut file = OpenOptions::new().write(true).open(path).map_err(|e| {
        bootycall_log::error!("Failed to open LED path {}: {:?}", path, e);
        e
    })?;
    write!(file, "{}", value).map_err(|e| {
        bootycall_log::error!(
            "Failed to write value {} to LED path {}: {:?}",
            value,
            path,
            e
        );
        e
    })
}

/// Write `value` to `path` only when it differs from the last-known-good
/// value. BUG-14: the cache is only updated on a successful write —
/// previously a failed write still poisoned `last_value`, so subsequent
/// calls thought the hardware was already in the right state and never
/// retried.
fn set_led_cached(path: &str, value: u8, last_value: &mut Option<u8>) {
    set_led_cached_with(path, value, last_value, set_led);
}

/// Same as `set_led_cached` but takes the "actually write" closure as a
/// parameter so unit tests can exercise the caching-on-success logic
/// without touching `/sys/class/leds/*`.
fn set_led_cached_with<F>(path: &str, value: u8, last_value: &mut Option<u8>, mut writer: F)
where
    F: FnMut(&str, u8) -> std::io::Result<()>,
{
    if Some(value) == *last_value {
        return;
    }
    if writer(path, value).is_ok() {
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

/// Blinks the white LED to indicate booting/initialization.
/// The `stop_rx` channel should be sent a message when booting is complete.
pub async fn run_boot_blink(mut stop_rx: tokio::sync::mpsc::Receiver<()>) {
    let mut state = false;
    loop {
        tokio::select! {
            _ = stop_rx.recv() => {
                break;
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

    // Once stopped, turn solid blue to indicate ready
    activate_blue_led();
}

/// Background LED manager that polls StateStore for active deployments
/// and blinks Blue if active, or stays solid Blue if idle.
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
pub fn led_test(color: &str, blinking: bool) -> Result<(), anyhow::Error> {
    // Resolve which sysfs path is driven "on" (255) for the requested colour;
    // the paired path is always driven to 0. `off` drives both to 0 (no "on"
    // path). This collapses the previously duplicated blue/white arms — which
    // differed only in which path got 255 — into one solid/blink code path.
    let (on_path, off_path): (Option<&str>, &str) = match color {
        "blue" => (Some(LED_BLUE_PATH), LED_WHITE_PATH),
        "white" => (Some(LED_WHITE_PATH), LED_BLUE_PATH),
        "off" => (None, LED_WHITE_PATH),
        _ => {
            return Err(anyhow::anyhow!(
                "Unknown LED color: {}. Valid colors are blue, white, off.",
                color
            ));
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

    #[test]
    fn cache_updates_on_successful_write() {
        let mut last = None;
        let mut writes = Vec::new();
        set_led_cached_with("led", 255, &mut last, |_, v| {
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
        set_led_cached_with("led", 255, &mut last, |_, v| {
            writes.push(v);
            Err(std::io::Error::other("boom"))
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
        set_led_cached_with("led", 255, &mut last, |_, v| {
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
        set_led_cached_with("led", 255, &mut last, |_, _| {
            attempts += 1;
            Err(std::io::Error::other("boom"))
        });
        set_led_cached_with("led", 255, &mut last, |_, _| {
            attempts += 1;
            Ok(())
        });
        assert_eq!(attempts, 2, "second call must retry after a failed write");
        assert_eq!(last, Some(255));
    }
}
