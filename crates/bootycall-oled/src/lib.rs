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

const VISIBLE_Y_START: usize = 28;
const VISIBLE_HEIGHT: usize = 32;

pub async fn run_oled_manager(
    state_store: StateStore,
    mut shutdown_rx: tokio::sync::mpsc::Receiver<()>,
) -> Result<(), anyhow::Error> {
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

                    // Draw label (top aligned inside visible window)
                    renderer.draw_bitmap(12, VISIBLE_Y_START, icon, 16, 16);
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

        if let Err(e) = fb.flush() {
            error!("Failed to write to framebuffer: {:?}", e);
        }

        tokio::select! {
            _ = shutdown_rx.recv() => {
                info!("OLED Manager shutting down. Blanking screen...");
                fb.clear();
                let _ = fb.flush();
                break;
            }
            _ = sleep(Duration::from_millis(100)) => {}
        }
    }
    Ok(())
}

/// A pixel representation containing (x, y, intensity)
type TextPixel = (usize, usize, u8);

/// A text rendering result containing (width, height, pixels)
type RenderedText = (usize, usize, Vec<TextPixel>);

/// Layout helper for rendering a TTF/OTF font dynamically.
fn render_ttf_text(
    font_data: &[u8],
    text: &str,
    scale_px: f32,
) -> Result<RenderedText, anyhow::Error> {
    use rusttype::{Font, Scale, point};

    let font = Font::try_from_bytes(font_data)
        .ok_or_else(|| anyhow::anyhow!("Failed to parse font data"))?;
    let scale = Scale::uniform(scale_px);
    let v_metrics = font.v_metrics(scale);
    let mut glyphs = Vec::new();
    let mut caret: f32 = 0.0;
    for c in text.chars() {
        let base_glyph = font.glyph(c);
        let scaled = base_glyph.scaled(scale);
        let h_metrics = scaled.h_metrics();
        let positioned = scaled.positioned(point(caret.round(), v_metrics.ascent.round()));
        caret += h_metrics.advance_width + 1.0;
        glyphs.push(positioned);
    }

    let mut min_x = i32::MAX;
    let mut max_x = i32::MIN;
    let mut min_y = i32::MAX;
    let mut max_y = i32::MIN;

    let mut pixels = Vec::new();

    for glyph in &glyphs {
        if let Some(bb) = glyph.pixel_bounding_box() {
            if bb.min.x < min_x {
                min_x = bb.min.x;
            }
            if bb.max.x > max_x {
                max_x = bb.max.x;
            }
            if bb.min.y < min_y {
                min_y = bb.min.y;
            }
            if bb.max.y > max_y {
                max_y = bb.max.y;
            }

            glyph.draw(|x, y, v| {
                let px = (bb.min.x + x as i32) as usize;
                let py = (bb.min.y + y as i32) as usize;
                let intensity = (v * 255.0) as u8;
                if intensity > 64 {
                    pixels.push((px, py, 255));
                }
            });
        }
    }

    if pixels.is_empty() {
        return Ok((0, 0, Vec::new()));
    }

    let text_w = (max_x - min_x) as usize;
    let text_h = (max_y - min_y) as usize;

    let normalized_pixels = pixels
        .into_iter()
        .map(|(px, py, val)| {
            let norm_x = (px as i32 - min_x) as usize;
            let norm_y = (py as i32 - min_y) as usize;
            (norm_x, norm_y, val)
        })
        .collect();

    Ok((text_w, text_h, normalized_pixels))
}

/// Dynamic OLED rendering test for testing font size, family, and alignment.
pub fn oled_test(
    size: usize,
    alignment: &str,
    text: &str,
    font_path: Option<&str>,
) -> Result<(), anyhow::Error> {
    // Parse alignment parts (e.g., "center-top", "left-bottom", "center")
    let parts: Vec<&str> = alignment.split('-').collect();
    let horiz = parts.first().copied().unwrap_or("left");
    let vert = parts.get(1).copied().unwrap_or("middle");

    let mut fb = Framebuffer::new();
    fb.clear();

    {
        let mut renderer = Renderer::new(&mut fb);

        if let Some(path) = font_path {
            // Read and render custom TTF font
            let font_data = std::fs::read(path)?;
            let (text_w, text_h, pixels) = render_ttf_text(&font_data, text, size as f32)?;

            // Calculate x based on horizontal alignment
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

            // Calculate y based on vertical alignment inside the visible window
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

            // Draw custom rendered pixels
            for (px, py, intensity) in pixels {
                if x + px < WIDTH && y + py < HEIGHT {
                    fb.set_pixel(x + px, y + py, intensity);
                }
            }
        } else {
            use embedded_graphics::{
                mono_font::{ascii::{
                    FONT_4X6, FONT_5X7, FONT_5X8, FONT_6X9, FONT_6X10, FONT_6X12,
                    FONT_6X13, FONT_7X13, FONT_7X14, FONT_8X13, FONT_9X15, FONT_9X18,
                    FONT_10X20,
                }, MonoTextStyle},
                pixelcolor::BinaryColor,
                prelude::*,
                text::{Baseline, Text, TextStyleBuilder},
            };

            // Map target height size to nearest available MonoFont
            let font = match size {
                1..=6 => &FONT_4X6,
                7 => &FONT_5X7,
                8 => &FONT_5X8,
                9 => &FONT_6X9,
                10 => &FONT_6X10,
                11 | 12 => &FONT_6X12,
                13 => &FONT_6X13,
                14 => &FONT_7X14,
                15 | 16 => &FONT_9X15,
                17 | 18 => &FONT_9X18,
                _ => &FONT_10X20,
            };

            let text_style = MonoTextStyle::new(font, BinaryColor::On);
            let style = TextStyleBuilder::new().baseline(Baseline::Top).build();
            let text_obj = Text::with_text_style(text, Point::zero(), text_style, style);

            let text_w = text_obj.bounding_box().size.width as usize;
            let text_h = text_obj.bounding_box().size.height as usize;

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

            let text_obj = Text::with_text_style(text, Point::new(x as i32, y as i32), text_style, style);
            let _ = text_obj.draw(&mut fb);
        }
    }

    fb.flush()?;
    Ok(())
}
