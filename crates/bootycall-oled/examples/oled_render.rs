//! Render OLED metric-page samples to grayscale PNGs on the host so the font
//! output can be eyeballed off-device — in particular to compare before/after
//! the rusttype → ab_glyph migration.
//!
//! Usage: `cargo run -p bootycall-oled --example oled_render -- [out_dir]`
//! (defaults to `scratch/oled-baseline`, which is gitignored).
//!
//! The PNG encoding is shared with the `sim` feature via `bootycall_oled::png`
//! and is deliberately dependency-free (stored/uncompressed DEFLATE), so no
//! compression crate is pulled into this Nix-vendored workspace for a
//! debug-only tool.

use bootycall_oled::framebuffer::{HEIGHT, WIDTH};
use bootycall_oled::png::encode_gray_png;
use bootycall_oled::render_sample_to_gray;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "scratch/oled-baseline".to_string());
    std::fs::create_dir_all(&out_dir)?;

    // Values mirror the real metrics-page formats (see crates/.../metrics.rs):
    // uptime is "Xd Yh Zm", CPU usage "{:.1}%", RAM/DISK "{:.1}G / {:.1}G".
    let samples = [
        ("HOSTNAME", "bootycall"),
        ("IP ADDRESS", "192.168.1.100"),
        ("UPTIME", "3d 14h 22m"),
        ("CPU TEMP", "48.5C"),
        ("CPU USAGE", "12.0%"),
        ("RAM USAGE", "1.5G / 8.0G"),
        ("DISK USAGE", "18.5G / 50.0G"),
        ("KERNEL", "6.6.0"),
        ("GLYPHS", "ABCabc0123:.%/-"),
    ];

    for (label, value) in samples {
        let gray = render_sample_to_gray(label, value);
        let png = encode_gray_png(&gray, WIDTH, HEIGHT);
        let file = format!("{out_dir}/{}.png", label.replace(' ', "_").to_lowercase());
        std::fs::write(&file, png)?;
        println!("wrote {file}");
    }

    Ok(())
}
