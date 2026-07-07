use std::fs::File;
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;

pub const WIDTH: usize = 160;
pub const HEIGHT: usize = 60;
const FB_PATH: &str = "/dev/fb0";

pub struct Framebuffer {
    // 8-bit grayscale backbuffer
    buffer: [u8; WIDTH * HEIGHT],
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
        let mut lut = [0u16; 256];
        for (g, lut_entry) in lut.iter_mut().enumerate() {
            let r5 = (g >> 3) & 0x1F;
            let g6 = (g >> 2) & 0x3F;
            let b5 = (g >> 3) & 0x1F;
            let val = (r5 << 11) | (g6 << 5) | b5;
            // Swap bytes for little endian framebuffer
            *lut_entry = ((val as u16 & 0xFF) << 8) | ((val as u16 >> 8) & 0xFF);
        }

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
        let mut packed = Vec::with_capacity(WIDTH * HEIGHT * 2);
        if self.rotation == 0 {
            // Apply 180 degree rotation in user-space to cancel out the driver's 180 degree rotation
            for &gray in self.buffer.iter().rev() {
                let rgb565 = self.lut[gray as usize];
                packed.push((rgb565 & 0xFF) as u8);
                packed.push((rgb565 >> 8) as u8);
            }
        } else {
            // Unrotated in user-space (let the driver's 180 degree rotation apply)
            for &gray in self.buffer.iter() {
                let rgb565 = self.lut[gray as usize];
                packed.push((rgb565 & 0xFF) as u8);
                packed.push((rgb565 >> 8) as u8);
            }
        }

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

impl embedded_graphics::geometry::OriginDimensions for Framebuffer {
    fn size(&self) -> embedded_graphics::geometry::Size {
        embedded_graphics::geometry::Size::new(WIDTH as u32, HEIGHT as u32)
    }
}

impl embedded_graphics::draw_target::DrawTarget for Framebuffer {
    type Color = embedded_graphics::pixelcolor::BinaryColor;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = embedded_graphics::Pixel<Self::Color>>,
    {
        for embedded_graphics::Pixel(pos, color) in pixels.into_iter() {
            if pos.x >= 0 && pos.x < WIDTH as i32 && pos.y >= 0 && pos.y < HEIGHT as i32 {
                let color_val = match color {
                    embedded_graphics::pixelcolor::BinaryColor::On => 255,
                    embedded_graphics::pixelcolor::BinaryColor::Off => 0,
                };
                self.set_pixel(pos.x as usize, pos.y as usize, color_val);
            }
        }
        Ok(())
    }
}
