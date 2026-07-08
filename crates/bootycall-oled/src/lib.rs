pub mod assets;
pub mod framebuffer;
pub mod metrics;
pub mod renderer;

use crate::framebuffer::{Framebuffer, HEIGHT, WIDTH};
use crate::metrics::SystemMetrics;
use crate::renderer::Renderer;
use bootycall_core::state::StateStore;
use bootycall_log::{error, info};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const PAGE_DURATION: Duration = Duration::from_secs(3);
const SCREENSAVER_TIMEOUT: Duration = Duration::from_secs(120);
/// Cadence at which the render loop redraws. The shutdown flag is checked
/// once per tick, so this doubles as the shutdown latency ceiling.
const TICK: Duration = Duration::from_millis(1000);
/// How often to re-attempt acquiring the optional rackmount detect GPIO
/// line after a startup failure (e.g. a udev-rule race during boot). We
/// retry rather than pinning to "standalone" rotation for the whole
/// process lifetime.
const DETECT_RETRY_INTERVAL: Duration = Duration::from_secs(60);

use gpiocdev::line::Value;

/// Whether enough time has elapsed since the last detect-line acquisition
/// attempt to try again. Pulled out as a pure fn so the retry cadence is
/// unit-testable without real GPIO hardware.
fn should_retry_detect(since_last_attempt: Duration) -> bool {
    since_last_attempt >= DETECT_RETRY_INTERVAL
}

/// Attempt to acquire GPIO line 44 (rackmount detect) as an input. Returns
/// `None` when unavailable (optional hardware). Silent by design — callers
/// own the logging so it can be log-once rather than per-attempt.
fn request_detect_line() -> Option<gpiocdev::Request> {
    gpiocdev::Request::builder()
        .on_chip("/dev/gpiochip0")
        .with_line(44)
        .as_input()
        .request()
        .ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisplayMode {
    Screensaver,
    Metrics,
}

const VISIBLE_Y_START: usize = 28;
const VISIBLE_HEIGHT: usize = 32;

/// Metric-page descriptor. Adding a page is one entry: label, icon,
/// optional refresh callback (only pages backed by an expensive sysinfo
/// query need one), and a value getter. The old `%8` + triple `match`
/// over `page_index` scattered these across three call sites; the table
/// keeps them together.
struct PageSpec {
    label: &'static str,
    icon: &'static [u8; 256],
    refresh: Option<fn(&mut SystemMetrics)>,
    value: fn(&SystemMetrics) -> String,
}

const PAGES: &[PageSpec] = &[
    PageSpec {
        label: "HOSTNAME",
        icon: &crate::assets::ICON_HOST,
        refresh: None,
        value: |m| m.get_hostname(),
    },
    PageSpec {
        label: "IP ADDRESS",
        icon: &crate::assets::ICON_NETWORK,
        refresh: None,
        value: |m| m.get_ip_address(),
    },
    PageSpec {
        label: "UPTIME",
        icon: &crate::assets::ICON_CLOCK,
        refresh: None,
        value: |m| m.get_uptime(),
    },
    PageSpec {
        label: "CPU TEMP",
        icon: &crate::assets::ICON_HOST,
        refresh: Some(SystemMetrics::refresh_components),
        value: |m| m.get_cpu_temp(),
    },
    PageSpec {
        label: "CPU USAGE",
        icon: &crate::assets::ICON_HOST,
        refresh: Some(SystemMetrics::refresh_cpu),
        value: |m| m.get_cpu_usage(),
    },
    PageSpec {
        label: "RAM USAGE",
        icon: &crate::assets::ICON_HOST,
        refresh: Some(SystemMetrics::refresh_memory),
        value: |m| m.get_ram_usage(),
    },
    PageSpec {
        label: "DISK USAGE",
        icon: &crate::assets::ICON_HOST,
        refresh: Some(SystemMetrics::refresh_disks),
        value: |m| m.get_disk_usage(),
    },
    PageSpec {
        label: "KERNEL",
        icon: &crate::assets::ICON_HOST,
        refresh: None,
        value: |m| m.get_kernel(),
    },
];

/// Public entry point: spawn the sync render loop on a dedicated OS thread,
/// then wait for the async shutdown channel. Framebuffer I/O and sysinfo
/// refreshes are synchronous by nature; running them inside a tokio task
/// starves the reactor. A dedicated std::thread is cleaner than per-tick
/// spawn_blocking churn — and keeps this function's async signature, so the
/// caller in `main.rs` doesn't have to know.
pub async fn run_oled_manager(
    state_store: StateStore,
    mut shutdown_rx: tokio::sync::mpsc::Receiver<()>,
) -> Result<(), anyhow::Error> {
    info!("Starting OLED Manager Task...");

    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let flag_for_thread = shutdown_flag.clone();
    let state_for_thread = state_store.clone();

    let render_thread = std::thread::Builder::new()
        .name("bootycall-oled".to_string())
        .spawn(move || render_loop(state_for_thread, flag_for_thread))?;

    // Bridge the async shutdown channel to the sync loop.
    let _ = shutdown_rx.recv().await;
    shutdown_flag.store(true, Ordering::Relaxed);

    // Wait for the render thread to finish. Join is blocking; wrap it so we
    // don't stall the reactor while the last frame drains + screen blanks.
    match tokio::task::spawn_blocking(move || render_thread.join()).await {
        Ok(Ok(inner)) => inner,
        Ok(Err(_panic)) => Err(anyhow::anyhow!("OLED render thread panicked")),
        Err(join_err) => Err(anyhow::anyhow!("OLED shutdown join error: {join_err}")),
    }
}

fn render_loop(state_store: StateStore, shutdown: Arc<AtomicBool>) -> Result<(), anyhow::Error> {
    // Rackmount detect line (GPIO 44) is optional. Acquire it once at
    // startup; if it fails (e.g. a udev-rule race during boot) retry every
    // DETECT_RETRY_INTERVAL rather than pinning to standalone rotation for
    // the process lifetime. Log once on failure and once on recovery — never
    // per tick. Group permissions on /dev/gpiochip0 are handled via udev.
    let mut detect_request = request_detect_line();
    if detect_request.is_none() {
        info!("GPIO rackmount detection not available (optional); will retry periodically");
    }
    let mut last_detect_attempt = Instant::now();

    // Drive GPIO 46 high to enable power to the rackmount accessory slot.
    let _enable_request = gpiocdev::Request::builder()
        .on_chip("/dev/gpiochip0")
        .with_line(46)
        .as_output(Value::Active)
        .request()
        .map_err(|e| {
            info!(
                "GPIO rackmount power enable not available (optional): {:?}",
                e
            );
            e
        })
        .ok();

    let mut fb = Framebuffer::new();
    let mut sys_metrics = SystemMetrics::new();

    let mut last_activity = Instant::now();
    let mut current_mode = DisplayMode::Metrics;
    let mut page_index = 0;
    let mut last_page_flip = Instant::now();
    let mut ss_x = 0;
    let mut ss_y = VISIBLE_Y_START as isize;
    let mut ss_dx = 1;
    let mut ss_dy = 1;

    let mut last_ssh_check = Instant::now() - Duration::from_secs(10);
    let mut active_ssh = false;

    loop {
        let now = Instant::now();

        // 0. Retry the optional rackmount detect line if it wasn't available
        //    yet (log once on recovery, silent on continued failure).
        if detect_request.is_none() && should_retry_detect(now.duration_since(last_detect_attempt))
        {
            last_detect_attempt = now;
            detect_request = request_detect_line();
            if detect_request.is_some() {
                info!("GPIO rackmount detection now available");
            }
        }

        // 1. Only poll active SSH sessions every 5 seconds to prevent procfs spam
        if now.duration_since(last_ssh_check) >= Duration::from_secs(5) {
            sys_metrics.refresh_processes();
            active_ssh = sys_metrics.active_ssh_sessions() > 0;
            last_ssh_check = now;
        }

        // 2. Check for activity triggers
        let active_pxe = state_store.has_recent_activity(Duration::from_secs(30));

        if active_ssh || active_pxe {
            last_activity = now;
            current_mode = DisplayMode::Metrics;
        } else if last_activity.elapsed() > SCREENSAVER_TIMEOUT {
            current_mode = DisplayMode::Screensaver;
        }

        // 3. Only refresh display-specific metrics if we are in Metrics mode
        if current_mode == DisplayMode::Metrics {
            if last_page_flip.elapsed() > PAGE_DURATION {
                page_index = (page_index + 1) % PAGES.len();
                last_page_flip = now;
            }

            // Only refresh the subsystem that is currently being displayed.
            if let Some(refresh) = PAGES[page_index].refresh {
                refresh(&mut sys_metrics);
            }
        }

        fb.clear();

        {
            let mut renderer = Renderer::new(&mut fb);

            match current_mode {
                DisplayMode::Screensaver => {
                    // Update bounce logic (TARS text moves every 1 second).
                    // Measure "TARS" via the renderer instead of the old magic
                    // `29` (QUAL-7 overlap folded in here since it's the fix).
                    let width_tars = Renderer::measure_text("TARS", false) as isize;
                    let height_tars = 21isize;

                    let min_x = 0isize;
                    let max_x = WIDTH as isize - width_tars;
                    let min_y = VISIBLE_Y_START as isize;
                    let max_y = (HEIGHT as isize) - height_tars;

                    // Move first, then clamp. The old order clamped then
                    // moved, so the +dx/dy step could push `ss_x` to -1
                    // before the `ss_x as usize + 3` in draw_braille below
                    // panicked in debug builds. `.max(0)` on the draw
                    // coordinate is a belt-and-braces guard.
                    ss_x += ss_dx;
                    ss_y += ss_dy;

                    if ss_x >= max_x {
                        ss_x = max_x;
                        ss_dx = -ss_dx.abs();
                    } else if ss_x <= min_x {
                        ss_x = min_x;
                        ss_dx = ss_dx.abs();
                    }
                    if ss_y >= max_y {
                        ss_y = max_y;
                        ss_dy = -ss_dy.abs();
                    } else if ss_y <= min_y {
                        ss_y = min_y;
                        ss_dy = ss_dy.abs();
                    }

                    let draw_x = ss_x.max(0) as usize;
                    let draw_y = ss_y.max(min_y) as usize;
                    renderer.draw_text(draw_x, draw_y, "TARS", false);
                    renderer.draw_braille(draw_x + 3, draw_y + 13, 3, 3, 8);
                }
                DisplayMode::Metrics => {
                    let page = &PAGES[page_index % PAGES.len()];
                    let label = page.label;
                    let value = (page.value)(&sys_metrics);
                    let icon = page.icon;

                    // Draw label (top aligned inside visible window)
                    let icon_y = if label == "UPTIME" {
                        VISIBLE_Y_START + 1
                    } else {
                        VISIBLE_Y_START
                    };
                    renderer.draw_bitmap(12, icon_y, icon, 16, 16);
                    renderer.draw_text(32, VISIBLE_Y_START + 2, label, false);

                    // Draw separator in the middle of visible window
                    renderer.draw_line(
                        12,
                        VISIBLE_Y_START + 15,
                        WIDTH - 12,
                        VISIBLE_Y_START + 15,
                        128,
                    );

                    // Draw metric value (bottom aligned inside visible window)
                    let text_w = Renderer::measure_text(&value, true);
                    let val_x = if text_w < WIDTH {
                        (WIDTH - text_w) / 2
                    } else {
                        0
                    };
                    renderer.draw_text(val_x, VISIBLE_Y_START + 16, &value, true);
                }
            }
        }

        // Read rackmount detection state: low (0) = docked (rotation 0), high/error = standalone (rotation 180)
        let is_docked = if let Some(ref req) = detect_request {
            req.lone_value()
                .map(|val| val == Value::Inactive)
                .unwrap_or(false)
        } else {
            false
        };

        fb.rotation = if is_docked { 0 } else { 180 };

        if let Err(e) = fb.flush() {
            error!("Failed to write to framebuffer: {:?}", e);
        }

        if shutdown.load(Ordering::Relaxed) {
            info!("OLED Manager shutting down. Blanking screen...");
            fb.clear();
            let _ = fb.flush();
            break;
        }
        std::thread::sleep(TICK);
    }
    Ok(())
}

/// Dynamic OLED rendering test for testing font size and alignment using premium TrueType fonts.
pub fn oled_test(size: usize, alignment: &str, text: &str) -> Result<(), anyhow::Error> {
    // Parse alignment parts (e.g., "center-top", "left-bottom", "center")
    let parts: Vec<&str> = alignment.split('-').collect();
    let horiz = parts.first().copied().unwrap_or("left");
    let vert = parts.get(1).copied().unwrap_or("middle");

    let mut fb = Framebuffer::new();
    fb.clear();

    {
        let mut renderer = Renderer::new(&mut fb);
        let use_large_font = size > crate::renderer::BOLD_THRESHOLD;
        let text_w = Renderer::measure_text(text, use_large_font);
        // Text height matches the font point size rounded to whole pixels.
        let text_h = if use_large_font {
            crate::renderer::LARGE_SCALE as usize
        } else {
            crate::renderer::SMALL_SCALE as usize
        };

        let x = match horiz {
            "center" => {
                if text_w < WIDTH {
                    (WIDTH - text_w) / 2
                } else {
                    0
                }
            }
            "right" => {
                if text_w < WIDTH {
                    WIDTH - text_w - 5
                } else {
                    0
                }
            }
            _ => 5, // left
        };

        let y = match vert {
            "top" => VISIBLE_Y_START,
            "bottom" => {
                if text_h < VISIBLE_HEIGHT {
                    VISIBLE_Y_START + VISIBLE_HEIGHT - text_h
                } else {
                    VISIBLE_Y_START
                }
            }
            _ => {
                if text_h < VISIBLE_HEIGHT {
                    VISIBLE_Y_START + (VISIBLE_HEIGHT - text_h) / 2
                } else {
                    VISIBLE_Y_START
                }
            } // middle
        };

        renderer.draw_text(x, y, text, use_large_font);
    }

    fb.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_retry_waits_for_full_interval() {
        assert!(!should_retry_detect(Duration::from_secs(0)));
        assert!(!should_retry_detect(
            DETECT_RETRY_INTERVAL - Duration::from_millis(1)
        ));
        assert!(should_retry_detect(DETECT_RETRY_INTERVAL));
        assert!(should_retry_detect(
            DETECT_RETRY_INTERVAL + Duration::from_secs(30)
        ));
    }
}
