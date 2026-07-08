use std::fs::File;
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;

pub const WIDTH: usize = 160;
pub const HEIGHT: usize = 60;
const FB_PATH: &str = "/dev/fb0";

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

    pub fn flush(&mut self) -> std::io::Result<()> {
        let packed = pack_buffer(&self.buffer, &self.lut, self.rotation);

        if self.file.is_none() {
            self.file = OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_SYNC)
                .open(FB_PATH)
                .ok();
        }

        if let Some(ref mut file) = self.file {
            file.seek(SeekFrom::Start(0))?;
            file.write_all(&packed)?;
        } else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Framebuffer device file not available",
            ));
        }
        Ok(())
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
}
