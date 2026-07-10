use crate::framebuffer::{Framebuffer, HEIGHT, WIDTH};
use ab_glyph::{Font, FontRef, PxScale, ScaleFont, point};
use std::sync::OnceLock;

/// Font point size for regular (Lato) labels drawn on the OLED. Shared
/// with the layout math in `lib.rs` so the two files can't drift.
pub const SMALL_SCALE: f32 = 11.0;

/// Font point size for bold (Rajdhani) values drawn on the OLED.
pub const LARGE_SCALE: f32 = 15.0;

/// `oled_test --size` threshold: sizes strictly above this render bold,
/// at or below render regular. Kept next to the scale constants so
/// changing one nudges the code that switches between them.
pub const BOLD_THRESHOLD: usize = 13;

pub struct FontSet {
    pub regular: FontRef<'static>,
    pub bold: FontRef<'static>,
}

pub static FONTS: OnceLock<FontSet> = OnceLock::new();

pub fn get_fonts() -> &'static FontSet {
    FONTS.get_or_init(|| {
        let regular_bytes = include_bytes!("../assets/Lato-Regular.ttf");
        let bold_bytes = include_bytes!("../assets/Rajdhani-Bold.ttf");
        FontSet {
            regular: FontRef::try_from_slice(regular_bytes)
                .expect("Failed to load Lato-Regular.ttf"),
            bold: FontRef::try_from_slice(bold_bytes).expect("Failed to load Rajdhani-Bold.ttf"),
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
        let scale_px = if use_large_font {
            LARGE_SCALE
        } else {
            SMALL_SCALE
        };
        self.draw_text_scaled(x, y, text, use_large_font, scale_px);
    }

    /// Like [`draw_text`](Self::draw_text) but renders at an explicit point
    /// size rather than the fixed `SMALL_SCALE`/`LARGE_SCALE`. This lets
    /// `oled-test --size` actually drive the glyph scale; the production render
    /// loop keeps calling `draw_text` and is unaffected.
    pub fn draw_text_scaled(
        &mut self,
        x: usize,
        y: usize,
        text: &str,
        use_large_font: bool,
        scale_px: f32,
    ) {
        let font = if use_large_font {
            &get_fonts().bold
        } else {
            &get_fonts().regular
        };
        let scale = PxScale::from(scale_px);
        let scaled = font.as_scaled(scale);
        let baseline = y as f32 + scaled.ascent();

        // ab_glyph has no `font.layout` iterator, so advance the pen manually
        // per glyph (mirroring what rusttype did internally). Alpha coverage
        // is max-blended into the grayscale backbuffer, same as before.
        let mut caret = x as f32;
        for c in text.chars() {
            let glyph_id = font.glyph_id(c);
            let glyph = glyph_id.with_scale_and_position(scale, point(caret, baseline));
            if let Some(outlined) = font.outline_glyph(glyph) {
                let bounds = outlined.px_bounds();
                outlined.draw(|gx, gy, gv| {
                    let px = bounds.min.x as i32 + gx as i32;
                    let py = bounds.min.y as i32 + gy as i32;
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
            caret += scaled.h_advance(glyph_id);
        }
    }

    pub fn measure_text(text: &str, use_large_font: bool) -> usize {
        let scale_px = if use_large_font {
            LARGE_SCALE
        } else {
            SMALL_SCALE
        };
        Self::measure_text_scaled(text, use_large_font, scale_px)
    }

    /// Like [`measure_text`](Self::measure_text) but at an explicit point size,
    /// so `oled-test` can align text drawn via
    /// [`draw_text_scaled`](Self::draw_text_scaled).
    pub fn measure_text_scaled(text: &str, use_large_font: bool, scale_px: f32) -> usize {
        let font = if use_large_font {
            &get_fonts().bold
        } else {
            &get_fonts().regular
        };
        let scaled = font.as_scaled(PxScale::from(scale_px));
        let width: f32 = text
            .chars()
            .map(|c| scaled.h_advance(font.glyph_id(c)))
            .sum();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bresenham_horizontal_line_lights_only_row() {
        let mut fb = Framebuffer::new();
        {
            let mut r = Renderer::new(&mut fb);
            r.draw_line(3, 7, 8, 7, 200);
        }
        for x in 0..WIDTH {
            let expected = if (3..=8).contains(&x) { 200 } else { 0 };
            assert_eq!(
                fb.buffer[7 * WIDTH + x],
                expected,
                "row 7 col {x} unexpected"
            );
        }
        // Adjacent rows must stay dark.
        for x in 0..WIDTH {
            assert_eq!(fb.buffer[6 * WIDTH + x], 0);
            assert_eq!(fb.buffer[8 * WIDTH + x], 0);
        }
    }

    #[test]
    fn bresenham_diagonal_line_touches_expected_pixels() {
        let mut fb = Framebuffer::new();
        {
            let mut r = Renderer::new(&mut fb);
            r.draw_line(0, 0, 5, 5, 255);
        }
        for i in 0..=5 {
            assert_eq!(fb.buffer[i * WIDTH + i], 255, "diagonal ({i},{i}) missing");
        }
    }

    #[test]
    fn measure_text_empty_string_is_zero() {
        assert_eq!(Renderer::measure_text("", false), 0);
        assert_eq!(Renderer::measure_text("", true), 0);
    }

    #[test]
    fn measure_text_is_monotone_in_length() {
        // Not testing exact widths (they depend on the shipped fonts),
        // just the invariant that longer strings measure at least as
        // wide as shorter ones with the same font.
        let short = Renderer::measure_text("A", false);
        let long = Renderer::measure_text("AAAAAAAAAA", false);
        assert!(long >= short);
    }

    #[test]
    fn measure_text_bold_matches_regular_shape() {
        // Large font should also be non-zero for a non-empty string.
        assert!(Renderer::measure_text("HELLO", true) > 0);
    }

    #[test]
    fn measure_text_scaled_grows_with_point_size() {
        // The whole point of issue 015: the scale actually varies with size,
        // so a larger point size measures strictly wider.
        let small = Renderer::measure_text_scaled("HELLO", false, 6.0);
        let large = Renderer::measure_text_scaled("HELLO", false, 40.0);
        assert!(
            large > small,
            "larger point size must measure wider ({large} !> {small})"
        );
    }

    #[test]
    fn draw_text_scaled_lights_more_pixels_at_larger_size() {
        let lit_at = |px: f32| {
            let mut fb = Framebuffer::new();
            {
                let mut r = Renderer::new(&mut fb);
                r.draw_text_scaled(0, 0, "M", false, px);
            }
            fb.buffer.iter().filter(|&&p| p > 0).count()
        };
        assert!(
            lit_at(30.0) > lit_at(8.0),
            "a larger point size must light more pixels on the panel"
        );
    }
}
