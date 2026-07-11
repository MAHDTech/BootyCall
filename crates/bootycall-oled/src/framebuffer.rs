use std::fs::File;
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;

pub const WIDTH: usize = 160;
pub const HEIGHT: usize = 60;
pub const FB_PATH: &str = "/dev/fb0";
/// Bits per pixel the packing assumes (RGB565 → 2 bytes/pixel). Surfaced so a
/// geometry mismatch against the real panel can be diagnosed.
pub const BITS_PER_PIXEL: u32 = 16;

/// sysfs attribute directory for the primary framebuffer. Geometry is read
/// from here (plain-text `virtual_size` + `bits_per_pixel`) rather than via an
/// `FBIOGET_VSCREENINFO` ioctl: the workspace forbids `unsafe` (see
/// `Cargo.toml`), and the ioctl requires an unsafe FFI call. sysfs exposes the
/// same numbers with a safe `read_to_string`.
pub const FB_SYSFS_DIR: &str = "/sys/class/graphics/fb0";

/// Framebuffer geometry as reported by the kernel via sysfs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FbGeometry {
    pub xres: u32,
    pub yres: u32,
    pub bits_per_pixel: u32,
}

/// Parse the sysfs `virtual_size` (`"xres,yres"`) and `bits_per_pixel` strings
/// into an [`FbGeometry`]. Pure, so the parsing is unit-testable without a real
/// framebuffer. Returns `None` on any malformed field.
pub fn parse_fb_geometry(virtual_size: &str, bits_per_pixel: &str) -> Option<FbGeometry> {
    let (x, y) = virtual_size.trim().split_once(',')?;
    Some(FbGeometry {
        xres: x.trim().parse().ok()?,
        yres: y.trim().parse().ok()?,
        bits_per_pixel: bits_per_pixel.trim().parse().ok()?,
    })
}

/// Read the real panel geometry from the framebuffer's sysfs attributes.
/// Returns `None` when the device is absent or the attributes are unreadable
/// (not a framebuffer) — callers fall back to the compiled
/// `WIDTH`/`HEIGHT`/`BITS_PER_PIXEL` without panicking.
pub fn read_fb_geometry(sysfs_dir: &str) -> Option<FbGeometry> {
    let dir = std::path::Path::new(sysfs_dir);
    let virtual_size = std::fs::read_to_string(dir.join("virtual_size")).ok()?;
    let bits_per_pixel = std::fs::read_to_string(dir.join("bits_per_pixel")).ok()?;
    parse_fb_geometry(&virtual_size, &bits_per_pixel)
}

/// Full brightness — the identity LUT, i.e. the behaviour before brightness
/// control existed.
pub const FULL_BRIGHTNESS: u8 = 255;

pub struct Framebuffer {
    // 8-bit grayscale backbuffer
    pub buffer: [u8; WIDTH * HEIGHT],
    // LUT to convert grayscale to RGB565
    lut: [u16; 256],
    // Brightness the current LUT was built for (0–255).
    brightness: u8,
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
        Self::with_brightness(FULL_BRIGHTNESS)
    }

    /// Construct a framebuffer whose grayscale→RGB565 LUT is scaled by
    /// `brightness` (255 = full/unchanged, 0 = black).
    pub fn with_brightness(brightness: u8) -> Self {
        let lut = build_lut(brightness);

        let file = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_SYNC)
            .open(FB_PATH)
            .ok();

        Self {
            buffer: [0; WIDTH * HEIGHT],
            lut,
            brightness,
            file,
            rotation: 180,
        }
    }

    /// Rebuild the LUT for a new brightness. No-op when unchanged, so it is
    /// cheap to call every tick (used to dim the panel in screensaver mode).
    pub fn set_brightness(&mut self, brightness: u8) {
        if brightness != self.brightness {
            self.brightness = brightness;
            self.lut = build_lut(brightness);
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
        let Some(file) = self.file.as_mut() else {
            return Ok(false);
        };
        let packed = pack_buffer(&self.buffer, &self.lut, self.rotation);
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

/// Build the grayscale→RGB565 LUT, scaling each grayscale level by
/// `brightness`/255 before packing. `brightness == 255` is the identity map
/// (`Framebuffer::new`'s original behaviour); `0` blacks the panel out.
/// Exposed for tests so the invariant "black gray → 0x0000, white gray →
/// 0xFFFF (byte-swapped) at full brightness" can be verified.
pub fn build_lut(brightness: u8) -> [u16; 256] {
    let mut lut = [0u16; 256];
    for (g, lut_entry) in lut.iter_mut().enumerate() {
        // Scale the grayscale intensity by the brightness factor first.
        let scaled = (g as u32 * brightness as u32 / 255) as usize;
        let r5 = (scaled >> 3) & 0x1F;
        let g6 = (scaled >> 2) & 0x3F;
        let b5 = (scaled >> 3) & 0x1F;
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
        let lut = build_lut(FULL_BRIGHTNESS);
        assert_eq!(lut[0], 0, "gray 0 should pack to 0x0000");
        // Gray 255: r5 = 31, g6 = 63, b5 = 31 → 0xFFFF, then byte-swapped
        // stays 0xFFFF because it's palindromic in bytes.
        assert_eq!(lut[255], 0xFFFF, "gray 255 should pack to 0xFFFF");
    }

    #[test]
    fn lut_brightness_scales_intensity() {
        // brightness 0 blacks everything out, including white.
        let off = build_lut(0);
        assert_eq!(off[0], 0);
        assert_eq!(off[255], 0, "brightness 0 must black out even white");
        // Partial brightness dims white to neither full nor off.
        let half = build_lut(128);
        assert_ne!(half[255], 0xFFFF, "dimmed white must not be full");
        assert_ne!(half[255], 0, "dimmed white must not be black");
    }

    #[test]
    fn set_brightness_rebuilds_lut() {
        let mut fb = Framebuffer::with_brightness(FULL_BRIGHTNESS);
        assert_eq!(fb.lut, build_lut(FULL_BRIGHTNESS));
        fb.set_brightness(64);
        assert_eq!(fb.lut, build_lut(64));
        fb.set_brightness(FULL_BRIGHTNESS);
        assert_eq!(fb.lut, build_lut(FULL_BRIGHTNESS));
    }

    #[test]
    fn pack_rotation_180_yields_reversed_byte_order() {
        // A 4-pixel gradient: iterating forward vs reversed must produce
        // mirrored output.
        let lut = build_lut(FULL_BRIGHTNESS);
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
        let lut = build_lut(FULL_BRIGHTNESS);
        let buf = vec![0u8; 32];
        assert_eq!(pack_buffer(&buf, &lut, 0).len(), 64);
        assert_eq!(pack_buffer(&buf, &lut, 180).len(), 64);
    }

    #[test]
    fn parse_fb_geometry_reads_sysfs_format() {
        let g = parse_fb_geometry("160,60\n", "16\n").expect("well-formed");
        assert_eq!(
            g,
            FbGeometry {
                xres: 160,
                yres: 60,
                bits_per_pixel: 16
            }
        );
        // Malformed inputs → None, never a panic.
        assert!(parse_fb_geometry("160", "16").is_none()); // no comma
        assert!(parse_fb_geometry("garbage", "16").is_none());
        assert!(parse_fb_geometry("160,60", "xx").is_none());
    }

    #[test]
    fn geometry_is_none_when_sysfs_absent() {
        // Absent framebuffer sysfs dir → None (read fails), not a panic.
        assert!(read_fb_geometry("/nonexistent/graphics/fb0").is_none());
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
