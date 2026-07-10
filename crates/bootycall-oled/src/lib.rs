pub mod assets;
pub mod framebuffer;
pub mod metrics;
pub mod png;
pub mod renderer;

use crate::framebuffer::{Framebuffer, HEIGHT, WIDTH};
use crate::metrics::SystemMetrics;
use crate::renderer::Renderer;
use bootycall_core::state::StateStore;
use bootycall_log::{error, info, warn};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const PAGE_DURATION: Duration = Duration::from_secs(3);
/// Idle time before the screensaver kicks in. Shortened under the `sim` feature
/// so a brief off-device run still exercises the screensaver transition.
#[cfg(not(feature = "sim"))]
const SCREENSAVER_TIMEOUT: Duration = Duration::from_secs(120);
#[cfg(feature = "sim")]
const SCREENSAVER_TIMEOUT: Duration = Duration::from_secs(3);
/// Screensaver brightness as a percentage of the configured brightness — the
/// panel dims while idle to cut burn-in and power draw.
const SCREENSAVER_BRIGHTNESS_PERCENT: u16 = 40;
/// Inter-frame delay when running under the `sim` feature — faster than the
/// live 1 Hz tick so a capture completes quickly.
#[cfg(feature = "sim")]
const SIM_TICK: Duration = Duration::from_millis(100);
/// Cadence at which the render loop redraws. The shutdown flag is checked
/// once per tick, so this doubles as the shutdown latency ceiling.
const TICK: Duration = Duration::from_millis(1000);
/// How often to re-attempt acquiring an optional hardware resource (the GPIO
/// detect/power-enable lines) after a startup failure — e.g. a udev-rule race
/// during boot. We retry rather than pinning to a degraded state for the whole
/// process lifetime.
const OPTIONAL_HW_RETRY_INTERVAL: Duration = Duration::from_secs(60);

/// The GPIO character device the rackmount detect/power lines live on.
const GPIOCHIP_PATH: &str = "/dev/gpiochip0";

use gpiocdev::line::Value;

/// Whether enough time has elapsed since the last optional-hardware acquisition
/// attempt to try again. Pulled out as a pure fn so the retry cadence is
/// unit-testable without real hardware.
fn optional_hw_retry_due(since_last_attempt: Duration) -> bool {
    since_last_attempt >= OPTIONAL_HW_RETRY_INTERVAL
}

/// Whether any OLED/LED hardware is present. When neither the framebuffer nor
/// the GPIO chip node exists (a dev laptop, or a CloudKey with nothing wired),
/// the render loop runs headless instead of spinning uselessly at 1 Hz.
fn oled_hardware_present(fb_path: &str, gpiochip_path: &str) -> bool {
    std::path::Path::new(fb_path).exists() || std::path::Path::new(gpiochip_path).exists()
}

/// Attempt to acquire GPIO line 44 (rackmount detect) as an input. Returns
/// `None` when unavailable (optional hardware). Silent by design — callers
/// own the logging so it can be log-once rather than per-attempt.
fn request_detect_line() -> Option<gpiocdev::Request> {
    gpiocdev::Request::builder()
        .on_chip(GPIOCHIP_PATH)
        .with_line(44)
        .as_input()
        .request()
        .ok()
}

/// Attempt to acquire GPIO line 46 (rackmount power-enable) as an output driven
/// high, powering the accessory slot. Returns `None` when unavailable (optional
/// hardware). Silent for the same log-once reason as [`request_detect_line`].
fn request_power_enable_line() -> Option<gpiocdev::Request> {
    gpiocdev::Request::builder()
        .on_chip(GPIOCHIP_PATH)
        .with_line(46)
        .as_output(Value::Active)
        .request()
        .ok()
}

/// An optional GPIO line held for the process lifetime, with a shared
/// retry-and-log-once discipline. While the line is unavailable it is
/// re-requested every [`OPTIONAL_HW_RETRY_INTERVAL`] and logged once on
/// recovery; a still-failing request stays silent (no per-tick spam). The
/// guard is only re-requested while `None`, so a held line is never dropped
/// (which would revert its output). Shared by the detect and power-enable
/// lines so both behave identically.
struct OptionalGpioLine {
    request: Option<gpiocdev::Request>,
    last_attempt: Instant,
    acquire: fn() -> Option<gpiocdev::Request>,
    label: &'static str,
}

impl OptionalGpioLine {
    fn new(label: &'static str, acquire: fn() -> Option<gpiocdev::Request>, now: Instant) -> Self {
        let request = acquire();
        if request.is_none() {
            info!("{label} not available (optional); will retry periodically");
        }
        Self {
            request,
            last_attempt: now,
            acquire,
            label,
        }
    }

    /// Re-request the line if it is absent and the retry interval has elapsed.
    /// No-op while the line is held.
    fn poll(&mut self, now: Instant) {
        if self.request.is_none() && optional_hw_retry_due(now.duration_since(self.last_attempt)) {
            self.last_attempt = now;
            self.request = (self.acquire)();
            if self.request.is_some() {
                info!("{} now available", self.label);
            }
        }
    }

    fn get(&self) -> Option<&gpiocdev::Request> {
        self.request.as_ref()
    }
}

/// Where the render loop sends finished frames. Production drives the panel
/// (`/dev/fb0`); the `sim` feature dumps PNGs so the loop can run off-device.
/// Keeping this behind a trait lets the *real* `render_loop` — page rotation,
/// screensaver bounce, mode transitions, timing — run unchanged under sim.
trait FrameSink {
    /// Whether the loop should run given current hardware. Startup-only.
    fn should_run(&self) -> bool;
    /// One-time startup logging (panel geometry; nothing for sim).
    fn on_start(&self) {}
    /// Present the current backbuffer. Returns `false` to stop the loop (e.g.
    /// the sim frame budget is exhausted); the panel sink never stops itself.
    fn present(&mut self, fb: &mut Framebuffer) -> bool;
    /// Inter-frame delay.
    fn tick(&self) -> Duration;
}

/// Production sink: writes to the real framebuffer with the log-once
/// availability latch (BUG-C).
struct PanelSink {
    fb_available: bool,
}

impl PanelSink {
    fn new() -> Self {
        Self { fb_available: true }
    }
}

impl FrameSink for PanelSink {
    fn should_run(&self) -> bool {
        if oled_hardware_present(framebuffer::FB_PATH, GPIOCHIP_PATH) {
            true
        } else {
            info!(
                "OLED/LED hardware not detected (no {} or {}) — running headless",
                framebuffer::FB_PATH,
                GPIOCHIP_PATH
            );
            false
        }
    }

    fn on_start(&self) {
        // Validate the real panel geometry against the compiled assumption
        // once so a mismatch (which tears or is silently rejected on write) is
        // diagnosable from logs. Read-failure (absent/not a framebuffer) is
        // fine — we keep the compiled defaults.
        match framebuffer::read_fb_geometry(framebuffer::FB_SYSFS_DIR) {
            Some(geo) => {
                info!(
                    "Framebuffer geometry: {}x{}x{}bpp",
                    geo.xres, geo.yres, geo.bits_per_pixel
                );
                if geo.xres != WIDTH as u32
                    || geo.yres != HEIGHT as u32
                    || geo.bits_per_pixel != framebuffer::BITS_PER_PIXEL
                {
                    warn!(
                        "Framebuffer geometry {}x{}x{} differs from the compiled {}x{}x{} (RGB565); output may tear or be rejected",
                        geo.xres,
                        geo.yres,
                        geo.bits_per_pixel,
                        WIDTH,
                        HEIGHT,
                        framebuffer::BITS_PER_PIXEL
                    );
                }
            }
            None => {
                info!(
                    "Framebuffer geometry unavailable (device absent or not a framebuffer); using compiled {}x{}x{} defaults",
                    WIDTH,
                    HEIGHT,
                    framebuffer::BITS_PER_PIXEL
                );
            }
        }
    }

    fn present(&mut self, fb: &mut Framebuffer) -> bool {
        // Log-once on the framebuffer appearing/disappearing; a genuine write
        // error on an open fd is always logged (not the "absent" case).
        fb.ensure_open();
        match fb.write_packed() {
            Ok(true) => {
                if !self.fb_available {
                    info!("Framebuffer {} is now available", framebuffer::FB_PATH);
                    self.fb_available = true;
                }
            }
            Ok(false) => {
                if self.fb_available {
                    warn!(
                        "Framebuffer {} not available (optional hardware); suppressing further messages until it returns",
                        framebuffer::FB_PATH
                    );
                    self.fb_available = false;
                }
            }
            Err(e) => {
                error!("Failed to write to framebuffer: {:?}", e);
                self.fb_available = false;
            }
        }
        true
    }

    fn tick(&self) -> Duration {
        TICK
    }
}

/// Sim sink: dumps each frame as a grayscale PNG (`frame_NNNN.png`) into a
/// directory and stops after `max_frames`. Reuses the dependency-free encoder
/// in [`png`], so no windowing/compression crate is added to the build.
#[cfg(feature = "sim")]
struct SimSink {
    dir: std::path::PathBuf,
    frame: usize,
    max_frames: usize,
}

#[cfg(feature = "sim")]
impl FrameSink for SimSink {
    fn should_run(&self) -> bool {
        true
    }

    fn present(&mut self, fb: &mut Framebuffer) -> bool {
        if self.frame >= self.max_frames {
            return false;
        }
        let encoded = png::encode_gray_png(&fb.buffer, WIDTH, HEIGHT);
        let path = self.dir.join(format!("frame_{:04}.png", self.frame));
        if let Err(e) = std::fs::write(&path, encoded) {
            error!("sim: failed to write {}: {:?}", path.display(), e);
        }
        self.frame += 1;
        true
    }

    fn tick(&self) -> Duration {
        SIM_TICK
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisplayMode {
    Screensaver,
    Metrics,
}

/// Brightness to apply in a given mode: the full configured brightness for
/// Metrics, dimmed by [`SCREENSAVER_BRIGHTNESS_PERCENT`] for the screensaver.
/// Pure so the dim math is unit-testable.
fn mode_brightness(base: u8, mode: DisplayMode) -> u8 {
    match mode {
        DisplayMode::Metrics => base,
        DisplayMode::Screensaver => (base as u16 * SCREENSAVER_BRIGHTNESS_PERCENT / 100) as u8,
    }
}

const VISIBLE_Y_START: usize = 28;
const VISIBLE_HEIGHT: usize = 32;

// Metrics-page intra-page layout, in pixels, all relative to the visible
// window. The 16×16 status icon sits at the left margin; label and value text
// start past it; a separator line spans between symmetric left/right margins.
/// Left (and, mirrored as `WIDTH - `, right) margin for the icon and separator.
const METRICS_MARGIN_X: usize = 12;
/// X where label/value text begins — clear of the left-margin icon.
const METRICS_TEXT_X: usize = 32;
/// Label baseline offset below `VISIBLE_Y_START`.
const METRICS_LABEL_DY: usize = 2;
/// Separator-line offset below `VISIBLE_Y_START`.
const METRICS_SEPARATOR_DY: usize = 15;
/// Bold value baseline offset below `VISIBLE_Y_START`.
const METRICS_VALUE_DY: usize = 16;
/// Grayscale brightness (0–255) of the separator line.
const METRICS_SEPARATOR_BRIGHTNESS: u8 = 128;

/// Vertical extent (px) of the screensaver "TARS" glyph block plus the
/// braille dots drawn beneath it (`draw_y + 13` plus the dot rows). Used to
/// keep the bounce box inside the visible window. Named const rather than a
/// bare magic literal so the layout intent is documented in one place.
const SCREENSAVER_BLOCK_HEIGHT: isize = 21;

/// Bounding box the screensaver text reflects inside.
#[derive(Clone, Copy)]
struct BounceBox {
    min_x: isize,
    max_x: isize,
    min_y: isize,
    max_y: isize,
}

/// Advance the screensaver bounce one frame: move by `(dx, dy)`, then reflect
/// off the box walls, clamping the position back inside in the same frame it
/// moves. Returns the new `(x, y, dx, dy)`.
///
/// Pure so the `BUG-5` guarantee is unit-testable: the earlier clamp-then-move
/// ordering could leave `x` at `-1` before the `as usize` cast in
/// `draw_braille`, panicking in debug builds. Moving first then clamping keeps
/// `x`/`y` within bounds (and thus non-negative) at every draw point.
fn step_bounce(
    x: isize,
    y: isize,
    dx: isize,
    dy: isize,
    b: BounceBox,
) -> (isize, isize, isize, isize) {
    let mut x = x + dx;
    let mut y = y + dy;
    let mut dx = dx;
    let mut dy = dy;

    if x >= b.max_x {
        x = b.max_x;
        dx = -dx.abs();
    } else if x <= b.min_x {
        x = b.min_x;
        dx = dx.abs();
    }
    if y >= b.max_y {
        y = b.max_y;
        dy = -dy.abs();
    } else if y <= b.min_y {
        y = b.min_y;
        dy = dy.abs();
    }

    (x, y, dx, dy)
}

/// Metric-page descriptor. Adding a page is one entry: label, icon,
/// optional refresh callback (only pages backed by an expensive sysinfo
/// query need one), and a value getter. The old `%8` + triple `match`
/// over `page_index` scattered these across three call sites; the table
/// keeps them together.
struct PageSpec {
    label: &'static str,
    icon: &'static [u8; 256],
    /// Vertical nudge (px) applied to the icon so glyph-heavy icons line up
    /// with the label. Data-driven instead of a `label == "UPTIME"` special
    /// case at the draw site.
    icon_dy: usize,
    refresh: Option<fn(&mut SystemMetrics)>,
    value: fn(&SystemMetrics) -> String,
}

const PAGES: &[PageSpec] = &[
    PageSpec {
        label: "HOSTNAME",
        icon: &crate::assets::ICON_HOST,
        icon_dy: 0,
        refresh: None,
        value: |m| m.get_hostname(),
    },
    PageSpec {
        label: "IP ADDRESS",
        icon: &crate::assets::ICON_NETWORK,
        icon_dy: 0,
        refresh: None,
        value: |m| m.get_ip_address(),
    },
    PageSpec {
        label: "UPTIME",
        icon: &crate::assets::ICON_CLOCK,
        icon_dy: 1,
        refresh: None,
        value: |m| m.get_uptime(),
    },
    PageSpec {
        label: "CPU TEMP",
        icon: &crate::assets::ICON_HOST,
        icon_dy: 0,
        refresh: Some(SystemMetrics::refresh_components),
        value: |m| m.get_cpu_temp(),
    },
    PageSpec {
        label: "CPU USAGE",
        icon: &crate::assets::ICON_HOST,
        icon_dy: 0,
        refresh: Some(SystemMetrics::refresh_cpu),
        value: |m| m.get_cpu_usage(),
    },
    PageSpec {
        label: "RAM USAGE",
        icon: &crate::assets::ICON_HOST,
        icon_dy: 0,
        refresh: Some(SystemMetrics::refresh_memory),
        value: |m| m.get_ram_usage(),
    },
    PageSpec {
        label: "DISK USAGE",
        icon: &crate::assets::ICON_HOST,
        icon_dy: 0,
        refresh: Some(SystemMetrics::refresh_disks),
        value: |m| m.get_disk_usage(),
    },
    PageSpec {
        label: "KERNEL",
        icon: &crate::assets::ICON_HOST,
        icon_dy: 0,
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
    brightness: u8,
    mut shutdown_rx: tokio::sync::mpsc::Receiver<()>,
) -> Result<(), anyhow::Error> {
    info!("Starting OLED Manager Task...");

    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let flag_for_thread = shutdown_flag.clone();
    let state_for_thread = state_store.clone();

    let render_thread = std::thread::Builder::new()
        .name("bootycall-oled".to_string())
        .spawn(move || {
            render_loop(
                state_for_thread,
                brightness,
                flag_for_thread,
                PanelSink::new(),
            )
        })?;

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

fn render_loop<S: FrameSink>(
    state_store: StateStore,
    brightness: u8,
    shutdown: Arc<AtomicBool>,
    mut sink: S,
) -> Result<(), anyhow::Error> {
    // Hardware-absent (headless) short-circuit: the panel sink returns false
    // when neither the framebuffer nor the GPIO chip is present (a dev laptop,
    // or a CloudKey mid-boot), so we log once and idle instead of spinning the
    // loop to no-op. The sim sink always runs.
    if !sink.should_run() {
        return Ok(());
    }
    sink.on_start();

    // Rackmount detect (GPIO 44) and power-enable (GPIO 46) lines are optional
    // and coupled: without power the accessory slot is unpowered, so detect
    // always reads "standalone". Both share one retry-and-log-once discipline
    // (never per tick). Group permissions on the chip are handled via udev.
    let now0 = Instant::now();
    let mut detect_line =
        OptionalGpioLine::new("GPIO rackmount detection", request_detect_line, now0);
    let mut enable_line = OptionalGpioLine::new(
        "GPIO rackmount power enable",
        request_power_enable_line,
        now0,
    );

    let mut fb = Framebuffer::with_brightness(brightness);
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

        // 0. Retry the optional detect + power-enable lines while unavailable
        //    (log once on recovery, silent on continued failure).
        detect_line.poll(now);
        enable_line.poll(now);

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

        // Dim the panel while idle (no-op when the brightness is unchanged).
        fb.set_brightness(mode_brightness(brightness, current_mode));

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
                    // Measure "TARS" via the renderer instead of a magic width;
                    // the block height is a documented named const.
                    let width_tars = Renderer::measure_text("TARS", false) as isize;

                    let bounds = BounceBox {
                        min_x: 0,
                        max_x: WIDTH as isize - width_tars,
                        min_y: VISIBLE_Y_START as isize,
                        max_y: (HEIGHT as isize) - SCREENSAVER_BLOCK_HEIGHT,
                    };

                    // step_bounce moves then clamps, so ss_x/ss_y stay within
                    // bounds (and non-negative) at every draw point — see the
                    // BUG-5 note on the function. `.max(..)` below is a
                    // belt-and-braces guard on the usize cast.
                    (ss_x, ss_y, ss_dx, ss_dy) = step_bounce(ss_x, ss_y, ss_dx, ss_dy, bounds);

                    let draw_x = ss_x.max(0) as usize;
                    let draw_y = ss_y.max(bounds.min_y) as usize;
                    renderer.draw_text(draw_x, draw_y, "TARS", false);
                    renderer.draw_braille(draw_x + 3, draw_y + 13, 3, 3, 8);
                }
                DisplayMode::Metrics => {
                    let page = &PAGES[page_index % PAGES.len()];
                    let value = (page.value)(&sys_metrics);
                    draw_metrics_page(
                        &mut renderer,
                        page.label,
                        &value,
                        Some(page.icon),
                        page.icon_dy,
                    );
                }
            }
        }

        // Read rackmount detection state: low (0) = docked (rotation 0), high/error = standalone (rotation 180)
        let is_docked = if let Some(req) = detect_line.get() {
            req.lone_value()
                .map(|val| val == Value::Inactive)
                .unwrap_or(false)
        } else {
            false
        };

        fb.rotation = if is_docked { 0 } else { 180 };

        // Hand the frame to the sink (panel write, or PNG dump under sim). A
        // `false` return stops the loop (sim frame budget exhausted).
        if !sink.present(&mut fb) {
            break;
        }

        if shutdown.load(Ordering::Relaxed) {
            info!("OLED Manager shutting down. Blanking screen...");
            fb.clear();
            let _ = fb.flush();
            break;
        }
        std::thread::sleep(sink.tick());
    }
    Ok(())
}

/// Run the real render loop off-device under the `sim` feature, writing each
/// frame to a grayscale PNG (`frame_NNNN.png`) in `out_dir` and stopping after
/// `max_frames`. Lets page rotation, the screensaver bounce, and mode
/// transitions be inspected on a dev host without touching `/dev/fb0` or GPIO.
#[cfg(feature = "sim")]
pub fn run_sim(out_dir: &str, brightness: u8, max_frames: usize) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(out_dir)?;
    let state_store = StateStore::new();
    let shutdown = Arc::new(AtomicBool::new(false));
    let sink = SimSink {
        dir: std::path::PathBuf::from(out_dir),
        frame: 0,
        max_frames,
    };
    render_loop(state_store, brightness, shutdown, sink)
}

/// Draw a Metrics page: left status icon, top label, mid separator, and the
/// centred bold value. Shared by the live render loop and the PNG sample so
/// the two layouts cannot drift. When `icon` is `Some`, the icon and separator
/// are drawn (`icon_dy` nudges glyph-heavy icons into alignment); `None` draws
/// label + value only.
fn draw_metrics_page(
    renderer: &mut Renderer,
    label: &str,
    value: &str,
    icon: Option<&[u8; 256]>,
    icon_dy: usize,
) {
    if let Some(icon_bytes) = icon {
        renderer.draw_bitmap(
            METRICS_MARGIN_X,
            VISIBLE_Y_START + icon_dy,
            icon_bytes,
            16,
            16,
        );
        renderer.draw_line(
            METRICS_MARGIN_X,
            VISIBLE_Y_START + METRICS_SEPARATOR_DY,
            WIDTH - METRICS_MARGIN_X,
            VISIBLE_Y_START + METRICS_SEPARATOR_DY,
            METRICS_SEPARATOR_BRIGHTNESS,
        );
    }

    renderer.draw_text(
        METRICS_TEXT_X,
        VISIBLE_Y_START + METRICS_LABEL_DY,
        label,
        false,
    );

    let text_w = Renderer::measure_text(value, true);
    let val_x = if text_w < WIDTH {
        (WIDTH - text_w) / 2
    } else {
        0
    };
    renderer.draw_text(val_x, VISIBLE_Y_START + METRICS_VALUE_DY, value, true);
}

/// Small left/right edge inset (px) used by `oled_test`'s horizontal alignment.
const TEXT_EDGE_MARGIN: usize = 5;

/// Horizontal placement (px) for `oled_test`'s `--alignment`. `center` centres
/// within `WIDTH`, `right` right-aligns with an edge margin, anything else
/// (including `left`) uses the left margin. Text at least as wide as the panel
/// falls back to `0` so it is not pushed off-screen.
fn align_x(horiz: &str, text_w: usize) -> usize {
    match horiz {
        "center" => {
            if text_w < WIDTH {
                (WIDTH - text_w) / 2
            } else {
                0
            }
        }
        "right" => {
            if text_w < WIDTH {
                WIDTH - text_w - TEXT_EDGE_MARGIN
            } else {
                0
            }
        }
        _ => TEXT_EDGE_MARGIN, // left
    }
}

/// Vertical placement (px) within the visible window for `oled_test`. `top`
/// pins to the window top, `bottom` bottom-aligns, anything else (including
/// `middle`) centres. Text at least as tall as the window falls back to the
/// top.
fn align_y(vert: &str, text_h: usize) -> usize {
    match vert {
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
    }
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
        // `--size` drives the actual render scale (points ≈ pixels), not just
        // the bold/regular switch, so e.g. `--size 6` and `--size 40` visibly
        // differ. The production render loop keeps its fixed SMALL/LARGE scales.
        let scale_px = size as f32;
        let text_w = Renderer::measure_text_scaled(text, use_large_font, scale_px);
        // Glyph height ≈ the requested point size in pixels.
        let text_h = size;

        let x = align_x(horiz, text_w);
        let y = align_y(vert, text_h);

        renderer.draw_text_scaled(x, y, text, use_large_font, scale_px);
    }

    fb.flush()?;
    Ok(())
}

/// Render a full Metrics page (icon, label, separator, centred bold value) into
/// a fresh grayscale framebuffer and return the raw `WIDTH * HEIGHT` 8-bit
/// buffer. No `/dev/fb0` is touched, so this runs on any host — it powers the
/// `oled_render` example and its smoke test, which capture the rendered glyphs
/// to PNG for before/after font comparison (e.g. the rusttype → ab_glyph
/// migration). Shares `draw_metrics_page` with the live loop so the sample
/// matches the real panel.
pub fn render_sample_to_gray(label: &str, value: &str) -> Vec<u8> {
    let mut fb = Framebuffer::new();
    fb.clear();
    {
        let mut renderer = Renderer::new(&mut fb);
        draw_metrics_page(
            &mut renderer,
            label,
            value,
            Some(&crate::assets::ICON_HOST),
            0,
        );
    }
    fb.buffer.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_sample_has_expected_size_and_lit_pixels() {
        let gray = render_sample_to_gray("UPTIME", "3d 14:22:07");
        assert_eq!(gray.len(), WIDTH * HEIGHT);
        assert!(
            gray.iter().any(|&p| p > 0),
            "a rendered text sample should light some pixels"
        );
    }

    #[test]
    fn optional_hw_retry_waits_for_full_interval() {
        assert!(!optional_hw_retry_due(Duration::from_secs(0)));
        assert!(!optional_hw_retry_due(
            OPTIONAL_HW_RETRY_INTERVAL - Duration::from_millis(1)
        ));
        assert!(optional_hw_retry_due(OPTIONAL_HW_RETRY_INTERVAL));
        assert!(optional_hw_retry_due(
            OPTIONAL_HW_RETRY_INTERVAL + Duration::from_secs(30)
        ));
    }

    #[test]
    fn mode_brightness_dims_only_the_screensaver() {
        // Metrics uses the configured brightness unchanged.
        assert_eq!(mode_brightness(200, DisplayMode::Metrics), 200);
        assert_eq!(mode_brightness(255, DisplayMode::Metrics), 255);
        // Screensaver dims to the configured percentage.
        assert_eq!(
            mode_brightness(200, DisplayMode::Screensaver),
            (200 * SCREENSAVER_BRIGHTNESS_PERCENT / 100) as u8
        );
        // Brightness 0 stays 0 in both modes.
        assert_eq!(mode_brightness(0, DisplayMode::Screensaver), 0);
    }

    #[test]
    fn hardware_absent_when_neither_node_exists() {
        // Neither node present → headless.
        assert!(!oled_hardware_present(
            "/nonexistent/fb0",
            "/nonexistent/gpiochip0"
        ));
        // Either node present → run the loop. `/dev/null` always exists and
        // stands in for a present device node.
        assert!(oled_hardware_present("/dev/null", "/nonexistent/gpiochip0"));
        assert!(oled_hardware_present("/nonexistent/fb0", "/dev/null"));
    }

    #[test]
    fn align_x_covers_every_branch() {
        // A 40px string comfortably narrower than the 160px panel.
        assert_eq!(align_x("left", 40), TEXT_EDGE_MARGIN);
        assert_eq!(align_x("unknown", 40), TEXT_EDGE_MARGIN); // default → left
        assert_eq!(align_x("center", 40), (WIDTH - 40) / 2);
        assert_eq!(align_x("right", 40), WIDTH - 40 - TEXT_EDGE_MARGIN);
        // Overflow-guard fallbacks: text at least as wide as the panel → 0.
        assert_eq!(align_x("center", WIDTH), 0);
        assert_eq!(align_x("center", WIDTH + 10), 0);
        assert_eq!(align_x("right", WIDTH), 0);
    }

    #[test]
    fn align_y_covers_every_branch() {
        // A 15px line fits inside the 32px visible band.
        assert_eq!(align_y("top", 15), VISIBLE_Y_START);
        assert_eq!(align_y("bottom", 15), VISIBLE_Y_START + VISIBLE_HEIGHT - 15);
        assert_eq!(
            align_y("middle", 15),
            VISIBLE_Y_START + (VISIBLE_HEIGHT - 15) / 2
        );
        assert_eq!(
            align_y("unknown", 15),
            VISIBLE_Y_START + (VISIBLE_HEIGHT - 15) / 2
        ); // default → middle
        // Overflow-guard fallbacks: text at least as tall as the band → top.
        assert_eq!(align_y("bottom", VISIBLE_HEIGHT), VISIBLE_Y_START);
        assert_eq!(align_y("middle", VISIBLE_HEIGHT + 4), VISIBLE_Y_START);
    }

    #[test]
    fn step_bounce_reflects_off_left_wall_without_underflow() {
        // x=0 moving left (dx=-1): the pre-fix clamp-then-move ordering left
        // x at -1 (BUG-5). step_bounce must keep x at the wall and flip dx.
        let b = BounceBox {
            min_x: 0,
            max_x: 100,
            min_y: 5,
            max_y: 40,
        };
        let (x, _y, dx, _dy) = step_bounce(0, 10, -1, 1, b);
        assert_eq!(x, 0, "x must not underflow past the left wall");
        assert_eq!(dx, 1, "dx must flip to move right");
    }

    #[test]
    fn step_bounce_stays_in_bounds_for_many_frames() {
        let b = BounceBox {
            min_x: 0,
            max_x: 100,
            min_y: 5,
            max_y: 40,
        };
        // Start heading into the top-left corner — the worst case for BUG-5.
        let mut state = (0isize, 5isize, -1isize, -1isize);
        for _ in 0..1000 {
            state = step_bounce(state.0, state.1, state.2, state.3, b);
            assert!(
                state.0 >= b.min_x && state.0 <= b.max_x,
                "x out of bounds: {}",
                state.0
            );
            assert!(
                state.1 >= b.min_y && state.1 <= b.max_y,
                "y out of bounds: {}",
                state.1
            );
            // The draw coordinate casts to usize, so x/y must never be negative.
            assert!(state.0 >= 0 && state.1 >= 0);
        }
    }
}
