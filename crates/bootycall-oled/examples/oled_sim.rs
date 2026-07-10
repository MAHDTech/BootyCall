//! Run the real OLED render loop off-device under the `sim` feature, dumping
//! each frame to a grayscale PNG so page rotation, the screensaver bounce, and
//! mode transitions can be inspected on a laptop — the full loop, not the
//! single-frame `oled_render` sample.
//!
//! Usage:
//!   cargo run -p bootycall-oled --features sim --example oled_sim -- [out_dir] [frames]
//!
//! Defaults: out_dir = scratch/oled-sim (gitignored), frames = 120.

#[cfg(feature = "sim")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use bootycall_oled::framebuffer::FULL_BRIGHTNESS;

    let mut args = std::env::args().skip(1);
    let out_dir = args
        .next()
        .unwrap_or_else(|| "scratch/oled-sim".to_string());
    let frames: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(120);

    println!("Running OLED sim: {frames} frames → {out_dir}/");
    bootycall_oled::run_sim(&out_dir, FULL_BRIGHTNESS, frames)?;
    println!("done");
    Ok(())
}

#[cfg(not(feature = "sim"))]
fn main() {
    eprintln!(
        "This example requires the `sim` feature:\n  \
         cargo run -p bootycall-oled --features sim --example oled_sim"
    );
    std::process::exit(1);
}
