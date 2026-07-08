use crate::framebuffer::{Framebuffer, HEIGHT, WIDTH};
use rusttype::{Font, Scale, point};
use std::sync::OnceLock;

pub struct FontSet {
    pub regular: Font<'static>,
    pub bold: Font<'static>,
}

pub static FONTS: OnceLock<FontSet> = OnceLock::new();

pub fn get_fonts() -> &'static FontSet {
    FONTS.get_or_init(|| {
        let regular_bytes = include_bytes!("../assets/Lato-Regular.ttf");
        let bold_bytes = include_bytes!("../assets/Rajdhani-Bold.ttf");
        FontSet {
            regular: Font::try_from_bytes(regular_bytes).expect("Failed to load Lato-Regular.ttf"),
            bold: Font::try_from_bytes(bold_bytes).expect("Failed to load Rajdhani-Bold.ttf"),
        }
    })
}

pub struct Renderer<'a> {
    fb: &'a mut Framebuffer,
}

impl<'a> Renderer<'a> {
    pub fn new(fb: &'a mut Framebuffer) -> Self {
        Self { fb }
    }

    pub fn draw_rect(&mut self, x: usize, y: usize, w: usize, h: usize, color: u8) {
        for dy in 0..h {
            for dx in 0..w {
                self.fb.set_pixel(x + dx, y + dy, color);
            }
        }
    }

    pub fn draw_line(&mut self, x0: usize, y0: usize, x1: usize, y1: usize, color: u8) {
        let mut x = x0 as isize;
        let mut y = y0 as isize;
        let end_x = x1 as isize;
        let end_y = y1 as isize;

        let dx = (end_x - x).abs();
        let dy = -(end_y - y).abs();
        let sx = if x < end_x { 1 } else { -1 };
        let sy = if y < end_y { 1 } else { -1 };
        let mut err = dx + dy;

        loop {
            if x >= 0 && x < WIDTH as isize && y >= 0 && y < HEIGHT as isize {
                self.fb.set_pixel(x as usize, y as usize, color);
            }
            if x == end_x && y == end_y {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x += sx;
            }
            if e2 <= dx {
                err += dx;
                y += sy;
            }
        }
    }

    pub fn draw_bitmap(&mut self, x: usize, y: usize, bmp: &[u8], w: usize, h: usize) {
        for dy in 0..h {
            for dx in 0..w {
                if dy * w + dx < bmp.len() {
                    let pixel = bmp[dy * w + dx];
                    if pixel > 64 {
                        self.fb.set_pixel(x + dx, y + dy, pixel);
                    }
                }
            }
        }
    }

    pub fn draw_text(&mut self, x: usize, y: usize, text: &str, use_large_font: bool) {
        let font = if use_large_font {
            &get_fonts().bold
        } else {
            &get_fonts().regular
        };
        let scale_px = if use_large_font { 15.0 } else { 11.0 };
        let scale = Scale::uniform(scale_px);
        let v_metrics = font.v_metrics(scale);

        let glyphs: Vec<_> = font
            .layout(text, scale, point(x as f32, y as f32 + v_metrics.ascent))
            .collect();

        for glyph in glyphs {
            if let Some(bounding_box) = glyph.pixel_bounding_box() {
                glyph.draw(|gx, gy, gv| {
                    let px = bounding_box.min.x + gx as i32;
                    let py = bounding_box.min.y + gy as i32;
                    if px >= 0 && px < WIDTH as i32 && py >= 0 && py < HEIGHT as i32 {
                        let alpha = (gv * 255.0) as u8;
                        if alpha > 0 {
                            let current = self.fb.buffer[py as usize * WIDTH + px as usize];
                            let blended = std::cmp::max(current, alpha);
                            self.fb.set_pixel(px as usize, py as usize, blended);
                        }
                    }
                });
            }
        }
    }

    pub fn measure_text(text: &str, use_large_font: bool) -> usize {
        let font = if use_large_font {
            &get_fonts().bold
        } else {
            &get_fonts().regular
        };
        let scale_px = if use_large_font { 15.0 } else { 11.0 };
        let scale = Scale::uniform(scale_px);
        let glyphs: Vec<_> = font.layout(text, scale, point(0.0, 0.0)).collect();
        if glyphs.is_empty() {
            return 0;
        }
        let last_glyph = &glyphs[glyphs.len() - 1];
        let width = last_glyph.position().x + last_glyph.unpositioned().h_metrics().advance_width;
        width.ceil() as usize
    }

    // Braille renderer from python script
    pub fn draw_braille(
        &mut self,
        x_start: usize,
        y_start: usize,
        col_dist: usize,
        row_dist: usize,
        char_dist: usize,
    ) {
        let chars = [
            // T: dots 2,3,4,5
            [[false, true], [true, true], [true, false]],
            // A: dot 1
            [[true, false], [false, false], [false, false]],
            // R: dots 1,2,3,5
            [[true, false], [true, true], [true, false]],
            // S: dots 2,3,4
            [[false, true], [true, false], [true, false]],
        ];

        for (c_idx, char_grid) in chars.iter().enumerate() {
            let char_x = x_start + c_idx * char_dist;
            for (row_idx, row) in char_grid.iter().enumerate() {
                let dot_y = y_start + row_idx * row_dist;
                for (col_idx, &is_set) in row.iter().enumerate() {
                    let dot_x = char_x + col_idx * col_dist;
                    if is_set {
                        self.draw_rect(dot_x, dot_y, 2, 2, 255); // simple 2x2 square instead of ellipse for speed
                    }
                }
            }
        }
    }
}
