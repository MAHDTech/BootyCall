use bootycall_log::info;
use std::fs::OpenOptions;
use std::io::Write;
use tokio::time::{Duration, sleep};

const LED_BLUE_PATH: &str = "/sys/class/leds/blue/brightness";
const LED_WHITE_PATH: &str = "/sys/class/leds/white/brightness";

fn set_led(path: &str, value: u8) {
    match OpenOptions::new().write(true).open(path) {
        Ok(mut file) => {
            if let Err(e) = write!(file, "{}", value) {
                bootycall_log::error!(
                    "Failed to write value {} to LED path {}: {:?}",
                    value,
                    path,
                    e
                );
            }
        }
        Err(e) => {
            bootycall_log::error!("Failed to open LED path {}: {:?}", path, e);
        }
    }
}

pub fn activate_blue_led() {
    set_led(LED_BLUE_PATH, 255);
    set_led(LED_WHITE_PATH, 0);
    info!("LED set to solid Blue (Service Running)");
}

pub fn activate_white_led() {
    set_led(LED_WHITE_PATH, 255);
    set_led(LED_BLUE_PATH, 0);
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
                    set_led(LED_WHITE_PATH, 255);
                    set_led(LED_BLUE_PATH, 0);
                } else {
                    set_led(LED_WHITE_PATH, 0);
                    set_led(LED_BLUE_PATH, 0);
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
                        set_led(LED_BLUE_PATH, 255);
                        set_led(LED_WHITE_PATH, 0);
                    } else {
                        set_led(LED_BLUE_PATH, 0);
                        set_led(LED_WHITE_PATH, 0);
                    }
                    state = !state;
                } else {
                    set_led(LED_BLUE_PATH, 255);
                    set_led(LED_WHITE_PATH, 0);
                    state = true;
                }
            }
        }
    }

    // Set to solid white on clean shutdown
    activate_white_led();
}

/// Dynamic LED testing for testing color and blinking states.
pub fn led_test(color: &str, blinking: bool) -> Result<(), anyhow::Error> {
    match color {
        "blue" => {
            if blinking {
                info!("Testing Blinking Blue LED for 10 seconds...");
                for _ in 0..10 {
                    set_led(LED_BLUE_PATH, 255);
                    set_led(LED_WHITE_PATH, 0);
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    set_led(LED_BLUE_PATH, 0);
                    set_led(LED_WHITE_PATH, 0);
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
            } else {
                info!("Testing Solid Blue LED");
                set_led(LED_BLUE_PATH, 255);
                set_led(LED_WHITE_PATH, 0);
            }
        }
        "white" => {
            if blinking {
                info!("Testing Blinking White LED for 10 seconds...");
                for _ in 0..10 {
                    set_led(LED_WHITE_PATH, 255);
                    set_led(LED_BLUE_PATH, 0);
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    set_led(LED_WHITE_PATH, 0);
                    set_led(LED_BLUE_PATH, 0);
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
            } else {
                info!("Testing Solid White LED");
                set_led(LED_WHITE_PATH, 255);
                set_led(LED_BLUE_PATH, 0);
            }
        }
        "off" => {
            info!("Testing LEDs Off");
            set_led(LED_BLUE_PATH, 0);
            set_led(LED_WHITE_PATH, 0);
        }
        _ => {
            return Err(anyhow::anyhow!(
                "Unknown LED color: {}. Valid colors are blue, white, off.",
                color
            ));
        }
    }
    info!("LED test complete");
    Ok(())
}
