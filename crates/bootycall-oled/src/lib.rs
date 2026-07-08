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

use gpiocdev::line::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisplayMode {
    Screensaver,
    Metrics,
}

const VISIBLE_Y_START: usize = 28;
const VISIBLE_HEIGHT: usize = 32;

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
    // Try to open GPIO chip and line 44 for rackmount detection on startup.
    // Group permissions for video group on /dev/gpiochip0 are handled via udev rules.
    let detect_request = gpiocdev::Request::builder()
        .on_chip("/dev/gpiochip0")
        .with_line(44)
        .as_input()
        .request()
        .map_err(|e| {
            info!("GPIO rackmount detection not available (optional): {:?}", e);
            e
        })
        .ok();

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
                page_index = (page_index + 1) % 8; // 8 pages
                last_page_flip = now;
            }

            // Only refresh the subsystem that is currently being displayed!
            match page_index {
                3 => sys_metrics.refresh_components(), // CPU Temp
                4 => sys_metrics.refresh_cpu(),        // CPU Usage
                5 => sys_metrics.refresh_memory(),     // RAM Usage
                6 => sys_metrics.refresh_disks(),      // Disk Usage
                _ => {}                                // Others don't need refresh
            }
        }

        fb.clear();

        {
            let mut renderer = Renderer::new(&mut fb);

            match current_mode {
                DisplayMode::Screensaver => {
                    // Update bounce logic (TARS text moves every 1 second)
                    let width_tars = 29;
                    let height_tars = 21;

                    let min_y = VISIBLE_Y_START as isize;
                    let max_y = (HEIGHT - height_tars) as isize;

                    if ss_x + width_tars >= WIDTH as isize || ss_x <= 0 {
                        ss_dx = -ss_dx;
                    }
                    if ss_y >= max_y || ss_y <= min_y {
                        ss_dy = -ss_dy;
                    }

                    ss_x = ss_x.clamp(0, WIDTH as isize - width_tars);
                    ss_y = ss_y.clamp(min_y, max_y);

                    ss_x += ss_dx;
                    ss_y += ss_dy;

                    renderer.draw_text(ss_x as usize, ss_y as usize, "TARS", false);
                    renderer.draw_braille(ss_x as usize + 3, ss_y as usize + 13, 3, 3, 8);
                }
                DisplayMode::Metrics => {
                    let (label, value, icon) = match page_index {
                        0 => (
                            "HOSTNAME",
                            sys_metrics.get_hostname(),
                            &crate::assets::ICON_HOST,
                        ),
                        1 => (
                            "IP ADDRESS",
                            sys_metrics.get_ip_address(),
                            &crate::assets::ICON_NETWORK,
                        ),
                        2 => (
                            "UPTIME",
                            sys_metrics.get_uptime(),
                            &crate::assets::ICON_CLOCK,
                        ),
                        3 => (
                            "CPU TEMP",
                            sys_metrics.get_cpu_temp(),
                            &crate::assets::ICON_HOST,
                        ),
                        4 => (
                            "CPU USAGE",
                            sys_metrics.get_cpu_usage(),
                            &crate::assets::ICON_HOST,
                        ),
                        5 => (
                            "RAM USAGE",
                            sys_metrics.get_ram_usage(),
                            &crate::assets::ICON_HOST,
                        ),
                        6 => (
                            "DISK USAGE",
                            sys_metrics.get_disk_usage(),
                            &crate::assets::ICON_HOST,
                        ),
                        7 => (
                            "KERNEL",
                            sys_metrics.get_kernel(),
                            &crate::assets::ICON_HOST,
                        ),
                        _ => ("UNKNOWN", String::new(), &crate::assets::ICON_HOST),
                    };

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
        let use_large_font = size > 13;
        let text_w = Renderer::measure_text(text, use_large_font);
        let text_h = if use_large_font { 15 } else { 11 };

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
