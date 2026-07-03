pub mod assets;
pub mod framebuffer;
pub mod metrics;
pub mod renderer;

use crate::framebuffer::{Framebuffer, HEIGHT, WIDTH};
use crate::metrics::SystemMetrics;
use crate::renderer::Renderer;
use bootycall_core::state::StateStore;
use bootycall_log::{error, info};
use std::time::{Duration, Instant};
use tokio::time::sleep;

const PAGE_DURATION: Duration = Duration::from_secs(3);
const SCREENSAVER_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisplayMode {
    Screensaver,
    Metrics,
}

pub async fn run_oled_manager(state_store: StateStore) -> Result<(), anyhow::Error> {
    info!("Starting OLED Manager Task...");

    let mut fb = Framebuffer::new();
    let mut sys_metrics = SystemMetrics::new();

    let mut last_activity = Instant::now();
    let mut current_mode = DisplayMode::Metrics;
    let mut page_index = 0;
    let mut last_page_flip = Instant::now();
    let mut ss_x = 0;
    let mut ss_y = 10;
    let mut ss_dx = 1;
    let mut ss_dy = 1;

    loop {
        sys_metrics.refresh();

        // 1. Check for activity triggers
        let active_ssh = sys_metrics.active_ssh_sessions() > 0;
        let active_pxe = state_store.has_recent_activity(Duration::from_secs(30)); // Assuming this method exists or we track it

        if active_ssh || active_pxe {
            last_activity = Instant::now();
            current_mode = DisplayMode::Metrics;
        } else if last_activity.elapsed() > SCREENSAVER_TIMEOUT {
            current_mode = DisplayMode::Screensaver;
        }

        fb.clear();

        {
            let mut renderer = Renderer::new(&mut fb);

            match current_mode {
                DisplayMode::Screensaver => {
                    // Update bounce logic
                    let width_tars = 60; // rough width of 'TARS' and braille
                    let height_tars = 30; // rough height

                    if ss_x + width_tars >= WIDTH as isize || ss_x <= 0 {
                        ss_dx = -ss_dx;
                    }
                    if ss_y + height_tars >= HEIGHT as isize || ss_y <= 0 {
                        ss_dy = -ss_dy;
                    }

                    ss_x += ss_dx;
                    ss_y += ss_dy;

                    renderer.draw_text(ss_x as usize, ss_y as usize, "TARS", true);
                    renderer.draw_braille(ss_x as usize + 5, ss_y as usize + 15, 4, 4, 12);
                }
                DisplayMode::Metrics => {
                    if last_page_flip.elapsed() > PAGE_DURATION {
                        page_index = (page_index + 1) % 8; // 8 pages
                        last_page_flip = Instant::now();
                    }

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

                    // Draw label (top aligned)
                    renderer.draw_bitmap(12, 10, icon, 16, 16);
                    renderer.draw_text(32, 12, label, true);

                    // Draw separator
                    renderer.draw_line(12, 30, WIDTH - 12, 30, 128);

                    // Draw metric value (bottom aligned)
                    let text_w = Renderer::measure_text(&value, true);
                    let val_x = if text_w < WIDTH {
                        (WIDTH - text_w) / 2
                    } else {
                        0
                    };
                    renderer.draw_text(val_x, 40, &value, true);
                }
            }
        }

        if let Err(e) = fb.flush() {
            error!("Failed to write to framebuffer: {:?}", e);
        }

        sleep(Duration::from_millis(100)).await;
    }
}
