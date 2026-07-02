use bootycall_log::info;
use std::fs::OpenOptions;
use std::io::Write;
use tokio::time::{sleep, Duration};

const LED_BLUE_PATH: &str = "/sys/class/leds/blue/brightness";
const LED_WHITE_PATH: &str = "/sys/class/leds/white/brightness";

fn set_led(path: &str, value: u8) {
    if let Ok(mut file) = OpenOptions::new().write(true).open(path) {
        let _ = write!(file, "{}", value);
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
