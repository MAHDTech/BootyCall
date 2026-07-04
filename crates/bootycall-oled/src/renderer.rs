use crate::framebuffer::{Framebuffer, HEIGHT, WIDTH};

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
        use embedded_graphics::{
            mono_font::{
                MonoTextStyleBuilder,
                ascii::{FONT_6X12, FONT_9X15},
            },
            pixelcolor::BinaryColor,
            prelude::*,
            text::{Baseline, Text, TextStyleBuilder},
        };

        let font = if use_large_font {
            &FONT_9X15
        } else {
            &FONT_6X12
        };
        let text_style = MonoTextStyleBuilder::new()
            .font(font)
            .text_color(BinaryColor::On)
            .build();
        let style = TextStyleBuilder::new().baseline(Baseline::Top).build();

        let text_obj =
            Text::with_text_style(text, Point::new(x as i32, y as i32), text_style, style);
        let _ = text_obj.draw(self.fb);
    }

    pub fn measure_text(text: &str, use_large_font: bool) -> usize {
        use embedded_graphics::{
            mono_font::{
                MonoTextStyle,
                ascii::{FONT_6X12, FONT_9X15},
            },
            pixelcolor::BinaryColor,
            prelude::*,
            text::{Baseline, Text, TextStyleBuilder},
        };

        let font = if use_large_font {
            &FONT_9X15
        } else {
            &FONT_6X12
        };
        let text_style = MonoTextStyle::new(font, BinaryColor::On);
        let style = TextStyleBuilder::new().baseline(Baseline::Top).build();

        let text_obj = Text::with_text_style(text, Point::zero(), text_style, style);
        text_obj.bounding_box().size.width as usize
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
