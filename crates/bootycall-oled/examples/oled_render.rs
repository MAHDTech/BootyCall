//! Render OLED metric-page samples to grayscale PNGs on the host so the font
//! output can be eyeballed off-device — in particular to compare before/after
//! the rusttype → ab_glyph migration.
//!
//! Usage: `cargo run -p bootycall-oled --example oled_render -- [out_dir]`
//! (defaults to `scratch/oled-baseline`, which is gitignored).
//!
//! The PNG encoder below is deliberately dependency-free: it writes 8-bit
//! grayscale using *stored* (uncompressed) DEFLATE blocks, so no compression
//! crate is pulled into this Nix-vendored workspace for a debug-only tool.

use bootycall_oled::framebuffer::{HEIGHT, WIDTH};
use bootycall_oled::render_sample_to_gray;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "scratch/oled-baseline".to_string());
    std::fs::create_dir_all(&out_dir)?;

    // Representative label/value pairs plus a glyph-coverage string, chosen to
    // exercise the digits, punctuation and mixed case the real pages render.
    let samples = [
        ("HOSTNAME", "bootycall"),
        ("IP ADDRESS", "192.168.1.100"),
        ("UPTIME", "3d 14:22:07"),
        ("CPU TEMP", "48.5C"),
        ("CPU USAGE", "12%"),
        ("RAM USAGE", "512/2048 MB"),
        ("DISK USAGE", "37%"),
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

/// Encode an 8-bit grayscale buffer (`width * height` bytes) as a PNG using
/// stored DEFLATE blocks (no compression), so it needs no external crate.
fn encode_gray_png(gray: &[u8], width: usize, height: usize) -> Vec<u8> {
    assert_eq!(gray.len(), width * height);

    let mut out = Vec::new();
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);

    // IHDR: width, height, bit depth 8, colour type 0 (grayscale), no
    // compression/filter/interlace variants.
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(width as u32).to_be_bytes());
    ihdr.extend_from_slice(&(height as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 0, 0, 0, 0]);
    write_chunk(&mut out, b"IHDR", &ihdr);

    // Raw image data: each scanline prefixed with filter byte 0 (None).
    let mut raw = Vec::with_capacity(height * (1 + width));
    for row in gray.chunks(width) {
        raw.push(0);
        raw.extend_from_slice(row);
    }

    write_chunk(&mut out, b"IDAT", &zlib_store(&raw));
    write_chunk(&mut out, b"IEND", &[]);
    out
}

/// Wrap `data` in a zlib stream using stored (uncompressed) DEFLATE blocks.
fn zlib_store(data: &[u8]) -> Vec<u8> {
    let mut z = vec![0x78, 0x01]; // zlib header (no dict); 0x7801 % 31 == 0
    let mut offset = 0;
    while offset < data.len() {
        let chunk = &data[offset..(offset + 0xFFFF).min(data.len())];
        let is_final = offset + chunk.len() == data.len();
        z.push(if is_final { 1 } else { 0 }); // BFINAL bit, BTYPE=00 (stored)
        let len = chunk.len() as u16;
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(chunk);
        offset += chunk.len();
    }
    z.extend_from_slice(&adler32(data).to_be_bytes());
    z
}

fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc_input = Vec::with_capacity(4 + data.len());
    crc_input.extend_from_slice(kind);
    crc_input.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}
