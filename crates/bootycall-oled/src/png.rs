//! Dependency-free 8-bit grayscale PNG encoder.
//!
//! Writes PNG using *stored* (uncompressed) DEFLATE blocks, so no compression
//! crate is pulled into this Nix-vendored workspace for what is only ever a
//! debug/visualisation tool (the `oled_render` example and the `sim` feature's
//! frame dump). Shared so the two callers cannot drift.

/// Encode an 8-bit grayscale buffer (`width * height` bytes) as a PNG.
pub fn encode_gray_png(gray: &[u8], width: usize, height: usize) -> Vec<u8> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_png_magic_and_chunks() {
        let png = encode_gray_png(&[0u8; 4], 2, 2);
        // PNG signature.
        assert_eq!(
            &png[0..8],
            &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]
        );
        // IHDR/IDAT/IEND chunk type tags are all present.
        let has = |tag: &[u8; 4]| png.windows(4).any(|w| w == tag);
        assert!(has(b"IHDR") && has(b"IDAT") && has(b"IEND"));
    }
}
