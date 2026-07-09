use std::fs::File;
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;

pub const WIDTH: usize = 160;
pub const HEIGHT: usize = 60;
pub const FB_PATH: &str = "/dev/fb0";

pub struct Framebuffer {
    // 8-bit grayscale backbuffer
    pub buffer: [u8; WIDTH * HEIGHT],
    // LUT to convert grayscale to RGB565
    lut: [u16; 256],
    // Cached file descriptor to the framebuffer
    file: Option<File>,
    // Rotation state: 0 or 180 (defaults to 180 standalone)
    pub rotation: u16,
}

impl Default for Framebuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl Framebuffer {
    pub fn new() -> Self {
        let lut = build_lut();

        let file = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_SYNC)
            .open(FB_PATH)
            .ok();

        Self {
            buffer: [0; WIDTH * HEIGHT],
            lut,
            file,
            rotation: 180,
        }
    }

    pub fn clear(&mut self) {
        self.buffer.fill(0);
    }

    pub fn set_pixel(&mut self, x: usize, y: usize, color: u8) {
        if x < WIDTH && y < HEIGHT {
            self.buffer[y * WIDTH + x] = color;
        }
    }

    /// (Re)acquire the framebuffer fd if we don't already hold one. Returns
    /// `true` iff a usable fd is now held. Separated from [`write_packed`] so
    /// callers can distinguish "device never present" (`ensure_open` stays
    /// `false`) from "a write failed on an open fd" — the two want different
    /// logging (log-once vs every failure).
    pub fn ensure_open(&mut self) -> bool {
        if self.file.is_none() {
            self.file = OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_SYNC)
                .open(FB_PATH)
                .ok();
        }
        self.file.is_some()
    }

    /// Write the packed backbuffer to the held fd. Returns `Ok(true)` on a
    /// successful write, `Ok(false)` when no fd is held (device absent — call
    /// [`ensure_open`] first), and `Err` when the write itself fails on an
    /// open fd. A failed write drops the fd so the next [`ensure_open`]
    /// re-opens it.
    pub fn write_packed(&mut self) -> std::io::Result<bool> {
        if self.file.is_none() {
            return Ok(false);
        }
        let packed = pack_buffer(&self.buffer, &self.lut, self.rotation);
        let file = self.file.as_mut().expect("fd present (checked above)");
        if let Err(e) = file
            .seek(SeekFrom::Start(0))
            .and_then(|_| file.write_all(&packed))
        {
            // Drop the fd so a transient error re-opens on the next tick.
            self.file = None;
            return Err(e);
        }
        Ok(true)
    }

    pub fn flush(&mut self) -> std::io::Result<()> {
        self.ensure_open();
        if self.write_packed()? {
            Ok(())
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Framebuffer device file not available",
            ))
        }
    }
}

/// Pack an 8-bit grayscale buffer into byte-swapped RGB565 for the OLED
/// framebuffer. Split out of `Framebuffer::flush` so tests can exercise
/// the LUT + rotation invariants without needing `/dev/fb0`.
///
/// When `rotation == 0` the buffer is iterated in reverse so the
/// user-space 180° rotation cancels out the driver's; any other value
/// leaves the buffer un-rotated in user space.
pub fn pack_buffer(buffer: &[u8], lut: &[u16; 256], rotation: u16) -> Vec<u8> {
    let mut packed = Vec::with_capacity(buffer.len() * 2);
    if rotation == 0 {
        for &gray in buffer.iter().rev() {
            let rgb565 = lut[gray as usize];
            packed.push((rgb565 & 0xFF) as u8);
            packed.push((rgb565 >> 8) as u8);
        }
    } else {
        for &gray in buffer.iter() {
            let rgb565 = lut[gray as usize];
            packed.push((rgb565 & 0xFF) as u8);
            packed.push((rgb565 >> 8) as u8);
        }
    }
    packed
}

/// Construct the same LUT that `Framebuffer::new` builds. Exposed for
/// tests so the invariant "black gray → 0x0000, white gray → 0xFFFF
/// (byte-swapped)" can be verified.
pub fn build_lut() -> [u16; 256] {
    let mut lut = [0u16; 256];
    for (g, lut_entry) in lut.iter_mut().enumerate() {
        let r5 = (g >> 3) & 0x1F;
        let g6 = (g >> 2) & 0x3F;
        let b5 = (g >> 3) & 0x1F;
        let val = (r5 << 11) | (g6 << 5) | b5;
        // Swap bytes for little endian framebuffer
        *lut_entry = ((val as u16 & 0xFF) << 8) | ((val as u16 >> 8) & 0xFF);
    }
    lut
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lut_black_maps_to_zero_and_white_is_full() {
        let lut = build_lut();
        assert_eq!(lut[0], 0, "gray 0 should pack to 0x0000");
        // Gray 255: r5 = 31, g6 = 63, b5 = 31 → 0xFFFF, then byte-swapped
        // stays 0xFFFF because it's palindromic in bytes.
        assert_eq!(lut[255], 0xFFFF, "gray 255 should pack to 0xFFFF");
    }

    #[test]
    fn pack_rotation_180_yields_reversed_byte_order() {
        // A 4-pixel gradient: iterating forward vs reversed must produce
        // mirrored output.
        let lut = build_lut();
        let buf = [0u8, 64, 128, 255];
        let normal = pack_buffer(&buf, &lut, 180);
        let rotated = pack_buffer(&buf, &lut, 0);
        assert_eq!(normal.len(), 8);
        assert_eq!(rotated.len(), 8);
        // rotated byte-pairs are `normal` byte-pairs in reverse order.
        let normal_pairs: Vec<_> = normal.chunks(2).collect();
        let rotated_pairs: Vec<_> = rotated.chunks(2).collect();
        for (i, pair) in rotated_pairs.iter().enumerate() {
            assert_eq!(*pair, normal_pairs[normal_pairs.len() - 1 - i]);
        }
    }

    #[test]
    fn packed_length_is_two_bytes_per_pixel() {
        let lut = build_lut();
        let buf = vec![0u8; 32];
        assert_eq!(pack_buffer(&buf, &lut, 0).len(), 64);
        assert_eq!(pack_buffer(&buf, &lut, 180).len(), 64);
    }

    #[test]
    fn write_packed_reports_absence_without_error() {
        // On a host with no panel, write_packed must report Ok(false)
        // (absent), not Err — that is what lets the render loop log once
        // instead of erroring every tick (BUG-C).
        if std::path::Path::new(FB_PATH).exists() {
            return; // real panel present (rare in CI) — skip
        }
        let mut fb = Framebuffer::new();
        assert!(!fb.ensure_open(), "no {FB_PATH} → ensure_open is false");
        assert!(
            !fb.write_packed()
                .expect("absent device must be Ok(false), not Err"),
            "absent device → Ok(false)"
        );
    }
}
