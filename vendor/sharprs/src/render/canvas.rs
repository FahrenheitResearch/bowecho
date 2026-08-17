//! Self-contained RGBA pixel canvas with drawing primitives.
//!
//! Combines the best of rustdar's skew-T canvas (Wu's AA lines, bitmap font,
//! clipping) with wrf-rust-plots' wind barb renderer. No egui dependency.

use std::io::Cursor;
use std::sync::OnceLock;

use rusttype::{point, Font, Scale};

// BowEcho vendoring change (2026-08-16): upstream reads this font from the
// enclosing rusty-weather monorepo at
// `../../../../crates/rustwx-render/assets/fonts/SourceSans3-Regular.ttf`,
// which does not exist once the crate is vendored on its own. The identical
// font bytes now live inside the crate. See PATCHES.md.
const SOURCE_SANS_3_REGULAR: &[u8] = include_bytes!("../../assets/fonts/SourceSans3-Regular.ttf");

// ── Color type ──────────────────────────────────────────────────────

/// RGBA color as `[r, g, b, a]`.
pub type Color = [u8; 4];

// ── Text alignment ──────────────────────────────────────────────────

/// Horizontal text alignment for bitmap font text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

// ── 7×10 bitmap font ────────────────────────────────────────────────

pub const FONT_W: i32 = 7;
pub const FONT_H: i32 = 10;

static TEXT_FONT: OnceLock<Option<Font<'static>>> = OnceLock::new();

/// Return a 10-row bitmap for a character (7 bits wide per row).
pub fn char_bitmap(ch: char) -> [u16; 10] {
    match ch {
        '0' => [
            0b0011100, 0b0100010, 0b1000001, 0b1000101, 0b1001001, 0b1010001, 0b1000001, 0b0100010,
            0b0011100, 0b0000000,
        ],
        '1' => [
            0b0001000, 0b0011000, 0b0101000, 0b0001000, 0b0001000, 0b0001000, 0b0001000, 0b0001000,
            0b0111110, 0b0000000,
        ],
        '2' => [
            0b0111100, 0b1000010, 0b0000010, 0b0000100, 0b0001000, 0b0010000, 0b0100000, 0b1000000,
            0b1111110, 0b0000000,
        ],
        '3' => [
            0b0111100, 0b1000010, 0b0000010, 0b0011100, 0b0000010, 0b0000010, 0b0000010, 0b1000010,
            0b0111100, 0b0000000,
        ],
        '4' => [
            0b0000100, 0b0001100, 0b0010100, 0b0100100, 0b1000100, 0b1111110, 0b0000100, 0b0000100,
            0b0000100, 0b0000000,
        ],
        '5' => [
            0b1111110, 0b1000000, 0b1000000, 0b1111100, 0b0000010, 0b0000010, 0b0000010, 0b1000010,
            0b0111100, 0b0000000,
        ],
        '6' => [
            0b0011100, 0b0100000, 0b1000000, 0b1111100, 0b1000010, 0b1000010, 0b1000010, 0b0100010,
            0b0011100, 0b0000000,
        ],
        '7' => [
            0b1111110, 0b0000010, 0b0000100, 0b0001000, 0b0010000, 0b0010000, 0b0010000, 0b0010000,
            0b0010000, 0b0000000,
        ],
        '8' => [
            0b0111100, 0b1000010, 0b1000010, 0b0111100, 0b1000010, 0b1000010, 0b1000010, 0b1000010,
            0b0111100, 0b0000000,
        ],
        '9' => [
            0b0111100, 0b1000010, 0b1000010, 0b0111110, 0b0000010, 0b0000010, 0b0000100, 0b0001000,
            0b0110000, 0b0000000,
        ],
        'A' => [
            0b0011100, 0b0100010, 0b1000001, 0b1000001, 0b1111111, 0b1000001, 0b1000001, 0b1000001,
            0b1000001, 0b0000000,
        ],
        'B' => [
            0b1111100, 0b1000010, 0b1000010, 0b1111100, 0b1000010, 0b1000010, 0b1000010, 0b1000010,
            0b1111100, 0b0000000,
        ],
        'C' => [
            0b0011110, 0b0100001, 0b1000000, 0b1000000, 0b1000000, 0b1000000, 0b1000000, 0b0100001,
            0b0011110, 0b0000000,
        ],
        'D' => [
            0b1111100, 0b1000010, 0b1000001, 0b1000001, 0b1000001, 0b1000001, 0b1000001, 0b1000010,
            0b1111100, 0b0000000,
        ],
        'E' => [
            0b1111110, 0b1000000, 0b1000000, 0b1111100, 0b1000000, 0b1000000, 0b1000000, 0b1000000,
            0b1111110, 0b0000000,
        ],
        'F' => [
            0b1111110, 0b1000000, 0b1000000, 0b1111100, 0b1000000, 0b1000000, 0b1000000, 0b1000000,
            0b1000000, 0b0000000,
        ],
        'G' => [
            0b0011110, 0b0100001, 0b1000000, 0b1000000, 0b1001111, 0b1000001, 0b1000001, 0b0100001,
            0b0011110, 0b0000000,
        ],
        'H' => [
            0b1000001, 0b1000001, 0b1000001, 0b1111111, 0b1000001, 0b1000001, 0b1000001, 0b1000001,
            0b1000001, 0b0000000,
        ],
        'I' => [
            0b0111110, 0b0001000, 0b0001000, 0b0001000, 0b0001000, 0b0001000, 0b0001000, 0b0001000,
            0b0111110, 0b0000000,
        ],
        'J' => [
            0b0001111, 0b0000010, 0b0000010, 0b0000010, 0b0000010, 0b0000010, 0b1000010, 0b0100010,
            0b0011100, 0b0000000,
        ],
        'K' => [
            0b1000010, 0b1000100, 0b1001000, 0b1010000, 0b1100000, 0b1010000, 0b1001000, 0b1000100,
            0b1000010, 0b0000000,
        ],
        'L' => [
            0b1000000, 0b1000000, 0b1000000, 0b1000000, 0b1000000, 0b1000000, 0b1000000, 0b1000000,
            0b1111110, 0b0000000,
        ],
        'M' => [
            0b1000001, 0b1100011, 0b1010101, 0b1001001, 0b1000001, 0b1000001, 0b1000001, 0b1000001,
            0b1000001, 0b0000000,
        ],
        'N' => [
            0b1000001, 0b1100001, 0b1010001, 0b1001001, 0b1000101, 0b1000011, 0b1000001, 0b1000001,
            0b1000001, 0b0000000,
        ],
        'O' => [
            0b0011100, 0b0100010, 0b1000001, 0b1000001, 0b1000001, 0b1000001, 0b1000001, 0b0100010,
            0b0011100, 0b0000000,
        ],
        'P' => [
            0b1111100, 0b1000010, 0b1000010, 0b1111100, 0b1000000, 0b1000000, 0b1000000, 0b1000000,
            0b1000000, 0b0000000,
        ],
        'Q' => [
            0b0011100, 0b0100010, 0b1000001, 0b1000001, 0b1000001, 0b1000101, 0b1000010, 0b0100010,
            0b0011101, 0b0000000,
        ],
        'R' => [
            0b1111100, 0b1000010, 0b1000010, 0b1111100, 0b1010000, 0b1001000, 0b1000100, 0b1000010,
            0b1000001, 0b0000000,
        ],
        'S' => [
            0b0111110, 0b1000001, 0b1000000, 0b0111100, 0b0000010, 0b0000001, 0b0000001, 0b1000010,
            0b0111100, 0b0000000,
        ],
        'T' => [
            0b1111111, 0b0001000, 0b0001000, 0b0001000, 0b0001000, 0b0001000, 0b0001000, 0b0001000,
            0b0001000, 0b0000000,
        ],
        'U' => [
            0b1000001, 0b1000001, 0b1000001, 0b1000001, 0b1000001, 0b1000001, 0b1000001, 0b0100010,
            0b0011100, 0b0000000,
        ],
        'V' => [
            0b1000001, 0b1000001, 0b1000001, 0b0100010, 0b0100010, 0b0010100, 0b0010100, 0b0001000,
            0b0001000, 0b0000000,
        ],
        'W' => [
            0b1000001, 0b1000001, 0b1000001, 0b1000001, 0b1001001, 0b1010101, 0b1010101, 0b0100010,
            0b0100010, 0b0000000,
        ],
        'X' => [
            0b1000001, 0b0100010, 0b0010100, 0b0001000, 0b0001000, 0b0010100, 0b0100010, 0b1000001,
            0b1000001, 0b0000000,
        ],
        'Y' => [
            0b1000001, 0b0100010, 0b0010100, 0b0001000, 0b0001000, 0b0001000, 0b0001000, 0b0001000,
            0b0001000, 0b0000000,
        ],
        'Z' => [
            0b1111111, 0b0000010, 0b0000100, 0b0001000, 0b0010000, 0b0100000, 0b1000000, 0b1000000,
            0b1111111, 0b0000000,
        ],
        ' ' => [0; 10],
        ':' => [
            0b0000000, 0b0000000, 0b0001000, 0b0001000, 0b0000000, 0b0000000, 0b0001000, 0b0001000,
            0b0000000, 0b0000000,
        ],
        '.' => [
            0b0000000, 0b0000000, 0b0000000, 0b0000000, 0b0000000, 0b0000000, 0b0000000, 0b0001100,
            0b0001100, 0b0000000,
        ],
        '-' => [
            0b0000000, 0b0000000, 0b0000000, 0b0000000, 0b0111110, 0b0000000, 0b0000000, 0b0000000,
            0b0000000, 0b0000000,
        ],
        '+' => [
            0b0000000, 0b0000000, 0b0001000, 0b0001000, 0b0111110, 0b0001000, 0b0001000, 0b0000000,
            0b0000000, 0b0000000,
        ],
        '/' => [
            0b0000001, 0b0000010, 0b0000100, 0b0001000, 0b0010000, 0b0100000, 0b1000000, 0b0000000,
            0b0000000, 0b0000000,
        ],
        ',' => [
            0b0000000, 0b0000000, 0b0000000, 0b0000000, 0b0000000, 0b0000000, 0b0001100, 0b0001100,
            0b0000100, 0b0001000,
        ],
        '(' => [
            0b0000100, 0b0001000, 0b0010000, 0b0010000, 0b0010000, 0b0010000, 0b0010000, 0b0001000,
            0b0000100, 0b0000000,
        ],
        ')' => [
            0b0100000, 0b0010000, 0b0001000, 0b0001000, 0b0001000, 0b0001000, 0b0001000, 0b0010000,
            0b0100000, 0b0000000,
        ],
        '%' => [
            0b1100001, 0b1100010, 0b0000100, 0b0001000, 0b0010000, 0b0100000, 0b0100110, 0b1000110,
            0b0000000, 0b0000000,
        ],
        '=' => [
            0b0000000, 0b0000000, 0b0111110, 0b0000000, 0b0000000, 0b0111110, 0b0000000, 0b0000000,
            0b0000000, 0b0000000,
        ],
        '*' => [
            0b0000000, 0b0001000, 0b0101010, 0b0011100, 0b0101010, 0b0001000, 0b0000000, 0b0000000,
            0b0000000, 0b0000000,
        ],
        '~' => [
            0b0000000, 0b0000000, 0b0110000, 0b1001001, 0b0000110, 0b0000000, 0b0000000, 0b0000000,
            0b0000000, 0b0000000,
        ],
        '^' => [
            0b0001000, 0b0010100, 0b0100010, 0b0000000, 0b0000000, 0b0000000, 0b0000000, 0b0000000,
            0b0000000, 0b0000000,
        ],
        '_' => [
            0b0000000, 0b0000000, 0b0000000, 0b0000000, 0b0000000, 0b0000000, 0b0000000, 0b0000000,
            0b1111111, 0b0000000,
        ],
        '!' => [
            0b0001000, 0b0001000, 0b0001000, 0b0001000, 0b0001000, 0b0001000, 0b0000000, 0b0001000,
            0b0001000, 0b0000000,
        ],
        '?' => [
            0b0011100, 0b0100010, 0b0000010, 0b0000100, 0b0001000, 0b0001000, 0b0000000, 0b0001000,
            0b0001000, 0b0000000,
        ],
        '#' => [
            0b0010100, 0b0010100, 0b1111111, 0b0010100, 0b0010100, 0b1111111, 0b0010100, 0b0010100,
            0b0000000, 0b0000000,
        ],
        '[' => [
            0b0011100, 0b0010000, 0b0010000, 0b0010000, 0b0010000, 0b0010000, 0b0010000, 0b0010000,
            0b0011100, 0b0000000,
        ],
        ']' => [
            0b0011100, 0b0000100, 0b0000100, 0b0000100, 0b0000100, 0b0000100, 0b0000100, 0b0000100,
            0b0011100, 0b0000000,
        ],
        '<' => [
            0b0000010, 0b0000100, 0b0001000, 0b0010000, 0b0100000, 0b0010000, 0b0001000, 0b0000100,
            0b0000010, 0b0000000,
        ],
        '>' => [
            0b0100000, 0b0010000, 0b0001000, 0b0000100, 0b0000010, 0b0000100, 0b0001000, 0b0010000,
            0b0100000, 0b0000000,
        ],
        'a'..='z' => char_bitmap((ch as u8 - b'a' + b'A') as char),
        _ => [0; 10],
    }
}

// ═══════════════════════════════════════════════════════════════════
// Canvas
// ═══════════════════════════════════════════════════════════════════

/// RGBA pixel buffer with software drawing primitives.
///
/// All coordinates use `i32` (pixel) or `f64` (sub-pixel). Out-of-bounds
/// writes are silently clipped. Alpha blending uses source-over compositing.
pub struct Canvas {
    /// Raw RGBA pixel data, row-major, 4 bytes per pixel.
    pub pixels: Vec<u8>,
    /// Width in pixels.
    pub w: u32,
    /// Height in pixels.
    pub h: u32,
}

impl Canvas {
    // ── Construction ─────────────────────────────────────────────────

    /// Create a new canvas filled with the given background color.
    pub fn new(w: u32, h: u32, bg: Color) -> Self {
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        for i in 0..(w * h) as usize {
            pixels[i * 4] = bg[0];
            pixels[i * 4 + 1] = bg[1];
            pixels[i * 4 + 2] = bg[2];
            pixels[i * 4 + 3] = bg[3];
        }
        Self { pixels, w, h }
    }

    /// Create a new canvas with a transparent black background.
    pub fn new_transparent(w: u32, h: u32) -> Self {
        Self {
            pixels: vec![0u8; (w * h * 4) as usize],
            w,
            h,
        }
    }

    // ── Pixel access ────────────────────────────────────────────────

    /// Write a single pixel with source-over alpha blending.
    #[inline]
    pub fn put_pixel_blend(&mut self, x: i32, y: i32, col: Color) {
        if x < 0 || y < 0 || x >= self.w as i32 || y >= self.h as i32 {
            return;
        }
        let a = col[3];
        if a == 0 {
            return;
        }
        let idx = (y as u32 * self.w + x as u32) as usize * 4;
        if a == 255 {
            self.pixels[idx] = col[0];
            self.pixels[idx + 1] = col[1];
            self.pixels[idx + 2] = col[2];
            self.pixels[idx + 3] = 255;
            return;
        }
        let alpha = a as f32 / 255.0;
        let inv = 1.0 - alpha;
        self.pixels[idx] = (col[0] as f32 * alpha + self.pixels[idx] as f32 * inv) as u8;
        self.pixels[idx + 1] = (col[1] as f32 * alpha + self.pixels[idx + 1] as f32 * inv) as u8;
        self.pixels[idx + 2] = (col[2] as f32 * alpha + self.pixels[idx + 2] as f32 * inv) as u8;
        self.pixels[idx + 3] = 255;
    }

    /// Write a single pixel without blending (overwrite).
    #[inline]
    pub fn put_pixel(&mut self, x: i32, y: i32, col: Color) {
        if x < 0 || y < 0 || x >= self.w as i32 || y >= self.h as i32 {
            return;
        }
        let idx = (y as u32 * self.w + x as u32) as usize * 4;
        self.pixels[idx] = col[0];
        self.pixels[idx + 1] = col[1];
        self.pixels[idx + 2] = col[2];
        self.pixels[idx + 3] = col[3];
    }

    /// Read a pixel color. Returns `[0,0,0,0]` if out of bounds.
    #[inline]
    pub fn get_pixel(&self, x: i32, y: i32) -> Color {
        if x < 0 || y < 0 || x >= self.w as i32 || y >= self.h as i32 {
            return [0, 0, 0, 0];
        }
        let idx = (y as u32 * self.w + x as u32) as usize * 4;
        [
            self.pixels[idx],
            self.pixels[idx + 1],
            self.pixels[idx + 2],
            self.pixels[idx + 3],
        ]
    }

    // ── Lines ───────────────────────────────────────────────────────

    /// Wu's antialiased line drawing.
    pub fn draw_line_aa(&mut self, x0: f64, y0: f64, x1: f64, y1: f64, col: Color) {
        if !x0.is_finite() || !y0.is_finite() || !x1.is_finite() || !y1.is_finite() {
            return;
        }
        let steep = (y1 - y0).abs() > (x1 - x0).abs();
        let (mut x0, mut y0, mut x1, mut y1) = if steep {
            (y0, x0, y1, x1)
        } else {
            (x0, y0, x1, y1)
        };
        if x0 > x1 {
            std::mem::swap(&mut x0, &mut x1);
            std::mem::swap(&mut y0, &mut y1);
        }
        let dx = x1 - x0;
        let dy = y1 - y0;
        let gradient = if dx.abs() < 0.001 { 1.0 } else { dy / dx };

        let xend = x0.round();
        let yend = y0 + gradient * (xend - x0);
        let xpxl1 = xend as i32;
        let mut intery = yend + gradient;

        let xend2 = x1.round();
        let xpxl2 = xend2 as i32;

        if (xpxl2 - xpxl1).abs() > 10000 {
            return;
        }

        for x in xpxl1..=xpxl2 {
            let fpart = intery - intery.floor();
            let y = intery as i32;
            let a1 = ((1.0 - fpart) * col[3] as f64) as u8;
            let a2 = (fpart * col[3] as f64) as u8;
            if steep {
                self.put_pixel_blend(y, x, [col[0], col[1], col[2], a1]);
                self.put_pixel_blend(y + 1, x, [col[0], col[1], col[2], a2]);
            } else {
                self.put_pixel_blend(x, y, [col[0], col[1], col[2], a1]);
                self.put_pixel_blend(x, y + 1, [col[0], col[1], col[2], a2]);
            }
            intery += gradient;
        }
    }

    /// Bresenham line (for grid/utility lines).
    pub fn draw_line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, col: Color) {
        let dx = (x1 - x0).abs();
        let dy = -(y1 - y0).abs();
        if dx > 10000 || dy.abs() > 10000 {
            return;
        }
        let sx: i32 = if x0 < x1 { 1 } else { -1 };
        let sy: i32 = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;
        let mut cx = x0;
        let mut cy = y0;
        let max_steps = dx.max(dy.abs()) + 1;
        let mut steps = 0;
        loop {
            self.put_pixel_blend(cx, cy, col);
            if cx == x1 && cy == y1 {
                break;
            }
            steps += 1;
            if steps > max_steps {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                cx += sx;
            }
            if e2 <= dx {
                err += dx;
                cy += sy;
            }
        }
    }

    /// Thick antialiased line (parallel Wu offsets).
    pub fn draw_thick_line_aa(
        &mut self,
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
        col: Color,
        thickness: i32,
    ) {
        let dx = x1 - x0;
        let dy = y1 - y0;
        let len = (dx * dx + dy * dy).sqrt().max(1.0);
        let nx = -dy / len;
        let ny = dx / len;
        for d in -(thickness / 2)..=(thickness / 2) {
            let off = d as f64;
            self.draw_line_aa(
                x0 + nx * off,
                y0 + ny * off,
                x1 + nx * off,
                y1 + ny * off,
                col,
            );
        }
    }

    /// Dashed antialiased line.
    pub fn draw_dashed_line(
        &mut self,
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
        col: Color,
        dash: f64,
        gap: f64,
    ) {
        if !x0.is_finite() || !y0.is_finite() || !x1.is_finite() || !y1.is_finite() {
            return;
        }
        let dx = x1 - x0;
        let dy = y1 - y0;
        let len = (dx * dx + dy * dy).sqrt();
        if len < 1.0 || len > 10000.0 {
            return;
        }
        let ux = dx / len;
        let uy = dy / len;
        let mut dist = 0.0;
        let mut on = true;
        while dist < len {
            let seg = if on { dash } else { gap };
            let end = (dist + seg).min(len);
            if on {
                let sx = x0 + ux * dist;
                let sy = y0 + uy * dist;
                let ex = x0 + ux * end;
                let ey = y0 + uy * end;
                self.draw_line_aa(sx, sy, ex, ey, col);
            }
            dist = end;
            on = !on;
        }
    }

    /// Thick dashed antialiased line.
    pub fn draw_thick_dashed_line(
        &mut self,
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
        col: Color,
        thickness: i32,
        dash: f64,
        gap: f64,
    ) {
        let dx = x1 - x0;
        let dy = y1 - y0;
        let len = (dx * dx + dy * dy).sqrt().max(1.0);
        let nx = -dy / len;
        let ny = dx / len;
        for d in -(thickness / 2)..=(thickness / 2) {
            let off = d as f64;
            self.draw_dashed_line(
                x0 + nx * off,
                y0 + ny * off,
                x1 + nx * off,
                y1 + ny * off,
                col,
                dash,
                gap,
            );
        }
    }

    /// Draw a polyline (sequence of connected segments).
    pub fn draw_polyline_aa(&mut self, points: &[(f64, f64)], col: Color, thickness: i32) {
        if points.len() < 2 {
            return;
        }
        for seg in points.windows(2) {
            let (x0, y0) = seg[0];
            let (x1, y1) = seg[1];
            if thickness <= 1 {
                self.draw_line_aa(x0, y0, x1, y1, col);
            } else {
                self.draw_thick_line_aa(x0, y0, x1, y1, col, thickness);
            }
        }
    }

    // ── Spans & rectangles ──────────────────────────────────────────

    /// Fill a horizontal span with alpha blending.
    pub fn fill_span(&mut self, y: i32, x_left: i32, x_right: i32, col: Color) {
        if y < 0 || y >= self.h as i32 {
            return;
        }
        let l = x_left.max(0);
        let r = x_right.min(self.w as i32 - 1);
        for x in l..=r {
            self.put_pixel_blend(x, y, col);
        }
    }

    /// Fill a rectangle with alpha blending.
    pub fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, col: Color) {
        for row in y..y + h {
            self.fill_span(row, x, x + w - 1, col);
        }
    }

    /// Draw a rectangle outline.
    pub fn draw_rect(&mut self, x: i32, y: i32, w: i32, h: i32, col: Color) {
        self.draw_line(x, y, x + w - 1, y, col);
        self.draw_line(x, y + h - 1, x + w - 1, y + h - 1, col);
        self.draw_line(x, y, x, y + h - 1, col);
        self.draw_line(x + w - 1, y, x + w - 1, y + h - 1, col);
    }

    // ── Circles ─────────────────────────────────────────────────────

    /// Draw a circle outline (midpoint algorithm).
    pub fn draw_circle(&mut self, cx: i32, cy: i32, r: i32, col: Color) {
        let mut x = r;
        let mut y = 0;
        let mut err = 1 - r;
        while x >= y {
            self.put_pixel_blend(cx + x, cy + y, col);
            self.put_pixel_blend(cx - x, cy + y, col);
            self.put_pixel_blend(cx + x, cy - y, col);
            self.put_pixel_blend(cx - x, cy - y, col);
            self.put_pixel_blend(cx + y, cy + x, col);
            self.put_pixel_blend(cx - y, cy + x, col);
            self.put_pixel_blend(cx + y, cy - x, col);
            self.put_pixel_blend(cx - y, cy - x, col);
            y += 1;
            if err < 0 {
                err += 2 * y + 1;
            } else {
                x -= 1;
                err += 2 * (y - x) + 1;
            }
        }
    }

    /// Fill a circle (midpoint with scanline fill).
    pub fn fill_circle(&mut self, cx: i32, cy: i32, r: i32, col: Color) {
        let mut x = r;
        let mut y = 0;
        let mut err = 1 - r;
        while x >= y {
            self.fill_span(cy + y, cx - x, cx + x, col);
            self.fill_span(cy - y, cx - x, cx + x, col);
            self.fill_span(cy + x, cx - y, cx + y, col);
            self.fill_span(cy - x, cx - y, cx + y, col);
            y += 1;
            if err < 0 {
                err += 2 * y + 1;
            } else {
                x -= 1;
                err += 2 * (y - x) + 1;
            }
        }
    }

    // ── Triangle fill ───────────────────────────────────────────────

    /// Fill a triangle using barycentric rasterization with alpha blending.
    pub fn fill_triangle(
        &mut self,
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        col: Color,
    ) {
        let min_x = x0.min(x1).min(x2).floor() as i32;
        let max_x = x0.max(x1).max(x2).ceil() as i32;
        let min_y = y0.min(y1).min(y2).floor() as i32;
        let max_y = y0.max(y1).max(y2).ceil() as i32;

        let min_x = min_x.max(0);
        let max_x = max_x.min(self.w as i32 - 1);
        let min_y = min_y.max(0);
        let max_y = max_y.min(self.h as i32 - 1);

        let area2 = (x1 - x0) * (y2 - y0) - (x2 - x0) * (y1 - y0);
        if area2.abs() < 1e-6 {
            return;
        }
        let inv_area2 = 1.0 / area2;

        for py in min_y..=max_y {
            for px in min_x..=max_x {
                let pxf = px as f64 + 0.5;
                let pyf = py as f64 + 0.5;
                let w0 = ((x1 - pxf) * (y2 - pyf) - (x2 - pxf) * (y1 - pyf)) * inv_area2;
                let w1 = ((x2 - pxf) * (y0 - pyf) - (x0 - pxf) * (y2 - pyf)) * inv_area2;
                let w2 = 1.0 - w0 - w1;
                if w0 >= 0.0 && w1 >= 0.0 && w2 >= 0.0 {
                    self.put_pixel_blend(px, py, col);
                }
            }
        }
    }

    // ── Wind barbs (adapted from wrf-rust-plots) ────────────────────

    fn wind_barb_tail_unit_vector(u_screen: f64, v_screen: f64, speed: f64) -> (f64, f64) {
        (-u_screen / speed, -v_screen / speed)
    }

    /// Draw a meteorological wind barb.
    ///
    /// - `x_tip, y_tip`: position of the barb tip (observation point)
    /// - `u, v`: wind components in **screen** coordinates
    ///   (u right-positive, v down-positive) representing where the air is
    ///   moving. From met convention: `u_screen = -wspd*sin(wdir_rad)`,
    ///   `v_screen = wspd*cos(wdir_rad)`.
    /// - `shaft_len`: shaft length in pixels
    /// - `line_width`: line thickness
    pub fn draw_wind_barb(
        &mut self,
        x_tip: f64,
        y_tip: f64,
        u: f64,
        v: f64,
        col: Color,
        shaft_len: f64,
        line_width: i32,
    ) {
        if !u.is_finite() || !v.is_finite() {
            return;
        }

        let speed = (u * u + v * v).sqrt();
        if speed < 2.5 {
            self.fill_circle(x_tip.round() as i32, y_tip.round() as i32, 2, col);
            return;
        }

        let (tail_dx, tail_dy) = Self::wind_barb_tail_unit_vector(u, v, speed);
        let perp_dx = -tail_dy;
        let perp_dy = tail_dx;

        let tail_x = x_tip + tail_dx * shaft_len;
        let tail_y = y_tip + tail_dy * shaft_len;
        if line_width <= 1 {
            self.draw_line_aa(tail_x, tail_y, x_tip, y_tip, col);
        } else {
            self.draw_thick_line_aa(tail_x, tail_y, x_tip, y_tip, col, line_width);
        }

        let mut remaining = ((speed + 2.5) / 5.0).floor() as i32 * 5;
        let mut offset = shaft_len;
        let spacing = (shaft_len * 0.16).max(2.0);
        let full_height = shaft_len * 0.40;
        let full_width = shaft_len * 0.25;

        while remaining >= 50 {
            self.draw_barb_flag(
                x_tip,
                y_tip,
                tail_dx,
                tail_dy,
                perp_dx,
                perp_dy,
                offset,
                full_height,
                full_width,
                col,
                line_width,
            );
            remaining -= 50;
            offset -= full_width + spacing;
        }

        while remaining >= 10 {
            self.draw_barb_segment(
                x_tip,
                y_tip,
                tail_dx,
                tail_dy,
                perp_dx,
                perp_dy,
                offset,
                full_height,
                full_width * 0.5,
                col,
                line_width,
            );
            remaining -= 10;
            offset -= spacing;
        }

        if remaining >= 5 {
            if (offset - shaft_len).abs() < 1e-6 {
                offset -= 1.5 * spacing;
            }
            self.draw_barb_segment(
                x_tip,
                y_tip,
                tail_dx,
                tail_dy,
                perp_dx,
                perp_dy,
                offset,
                full_height * 0.5,
                full_width * 0.25,
                col,
                line_width,
            );
        }
    }

    /// Draw a wind barb from meteorological direction (degrees) and speed (knots).
    pub fn draw_wind_barb_met(
        &mut self,
        x: f64,
        y: f64,
        wdir_deg: f64,
        wspd_kt: f64,
        col: Color,
        shaft_len: f64,
        line_width: i32,
    ) {
        let dir_rad = wdir_deg.to_radians();
        let u = -wspd_kt * dir_rad.sin();
        let v = wspd_kt * dir_rad.cos();
        self.draw_wind_barb(x, y, u, v, col, shaft_len, line_width);
    }

    fn draw_barb_segment(
        &mut self,
        x_tip: f64,
        y_tip: f64,
        tail_dx: f64,
        tail_dy: f64,
        perp_dx: f64,
        perp_dy: f64,
        offset: f64,
        height: f64,
        along_tail: f64,
        col: Color,
        line_width: i32,
    ) {
        let base_x = x_tip + tail_dx * offset;
        let base_y = y_tip + tail_dy * offset;
        let feather_x = base_x + perp_dx * height + tail_dx * along_tail;
        let feather_y = base_y + perp_dy * height + tail_dy * along_tail;
        if line_width <= 1 {
            self.draw_line_aa(base_x, base_y, feather_x, feather_y, col);
        } else {
            self.draw_thick_line_aa(base_x, base_y, feather_x, feather_y, col, line_width);
        }
    }

    fn draw_barb_flag(
        &mut self,
        x_tip: f64,
        y_tip: f64,
        tail_dx: f64,
        tail_dy: f64,
        perp_dx: f64,
        perp_dy: f64,
        offset: f64,
        height: f64,
        width_along: f64,
        col: Color,
        line_width: i32,
    ) {
        let base_x = x_tip + tail_dx * offset;
        let base_y = y_tip + tail_dy * offset;
        let flag_tip_x = base_x + perp_dx * height - tail_dx * (width_along * 0.5);
        let flag_tip_y = base_y + perp_dy * height - tail_dy * (width_along * 0.5);
        let flag_tail_x = base_x - tail_dx * width_along;
        let flag_tail_y = base_y - tail_dy * width_along;
        self.fill_triangle(
            base_x,
            base_y,
            flag_tip_x,
            flag_tip_y,
            flag_tail_x,
            flag_tail_y,
            col,
        );
        let w = line_width + 1;
        self.draw_thick_line_aa(base_x, base_y, flag_tip_x, flag_tip_y, col, w);
        self.draw_thick_line_aa(flag_tip_x, flag_tip_y, flag_tail_x, flag_tail_y, col, w);
        self.draw_thick_line_aa(flag_tail_x, flag_tail_y, base_x, base_y, col, w);
    }

    // ── Bitmap font text (7×10) ─────────────────────────────────────

    /// Width of a text string in pixels using the built-in 7×10 font.
    pub fn text_width(text: &str) -> i32 {
        Self::text_width_scaled(text, 1)
    }

    pub fn text_width_scaled(text: &str, scale_tag: i32) -> i32 {
        let scale_tag = scale_tag.max(1);
        if let Some(font) = sounding_font() {
            measure_ttf_text(text, font_px(scale_tag), font) as i32
        } else {
            let n = text.len() as i32;
            if n == 0 {
                0
            } else {
                n * (FONT_W + 1) * scale_tag - scale_tag
            }
        }
    }

    /// Height of the built-in 7×10 font in pixels.
    pub const fn font_height() -> i32 {
        FONT_H
    }

    /// Draw a single 7×10 character at pixel position `(px, py)`.
    pub fn draw_char(&mut self, ch: char, px: i32, py: i32, col: Color) {
        let bitmap = char_bitmap(ch);
        for (row, &bits) in bitmap.iter().enumerate() {
            for col_idx in 0..FONT_W {
                if bits & (1 << (FONT_W - 1 - col_idx)) != 0 {
                    self.put_pixel_blend(px + col_idx, py + row as i32, col);
                }
            }
        }
    }

    /// Draw a text string at pixel position `(px, py)`.
    pub fn draw_text(&mut self, text: &str, px: i32, py: i32, col: Color) {
        self.draw_text_scaled(text, px, py, col, 1);
    }

    pub fn draw_text_scaled(&mut self, text: &str, px: i32, py: i32, col: Color, scale_tag: i32) {
        let scale_tag = scale_tag.max(1);
        if let Some(font) = sounding_font() {
            draw_ttf_text(self, text, px, py, col, font_px(scale_tag), font);
            return;
        }

        let mut x = px;
        for ch in text.chars() {
            self.draw_char(ch, x, py, col);
            x += (FONT_W + 1) * scale_tag;
        }
    }

    /// Draw text with the specified horizontal alignment.
    pub fn draw_text_aligned(
        &mut self,
        text: &str,
        anchor_x: i32,
        y: i32,
        col: Color,
        align: Align,
    ) {
        let w = Self::text_width(text);
        let x = match align {
            Align::Left => anchor_x,
            Align::Right => anchor_x - w,
            Align::Center => anchor_x - w / 2,
        };
        self.draw_text(text, x, y, col);
    }

    /// Draw text right-aligned.
    pub fn draw_text_right(&mut self, text: &str, right_x: i32, py: i32, col: Color) {
        self.draw_text_aligned(text, right_x, py, col, Align::Right);
    }

    /// Draw text centered horizontally.
    pub fn draw_text_centered(&mut self, text: &str, center_x: i32, py: i32, col: Color) {
        self.draw_text_aligned(text, center_x, py, col, Align::Center);
    }

    // ── PNG output ──────────────────────────────────────────────────

    /// Encode the canvas as a PNG and return the bytes.
    pub fn to_png(&self) -> Vec<u8> {
        let img = image::RgbaImage::from_raw(self.w, self.h, self.pixels.clone())
            .expect("pixel buffer size mismatch");
        let mut buf = Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Png)
            .expect("PNG encoding failed");
        buf.into_inner()
    }

    /// Save the canvas as a PNG file.
    pub fn save_png(&self, path: &str) -> std::io::Result<()> {
        let img = image::RgbaImage::from_raw(self.w, self.h, self.pixels.clone())
            .expect("pixel buffer size mismatch");
        img.save(path)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
    }

    // ── Sub-canvas clipping ─────────────────────────────────────────

    /// Create a [`ClippedCanvas`] that restricts all drawing to a sub-rectangle.
    pub fn clipped(&mut self, x: i32, y: i32, w: i32, h: i32) -> ClippedCanvas<'_> {
        ClippedCanvas::new(self, x, y, w, h)
    }
}

// ═══════════════════════════════════════════════════════════════════
// ClippedCanvas
// ═══════════════════════════════════════════════════════════════════

fn sounding_font() -> Option<&'static Font<'static>> {
    TEXT_FONT
        .get_or_init(|| Font::try_from_bytes(SOURCE_SANS_3_REGULAR))
        .as_ref()
}

fn font_px(scale_tag: i32) -> f32 {
    match scale_tag.max(1) {
        1 => 22.0,
        2 => 42.0,
        3 => 62.0,
        value => 22.0 + (value as f32 - 1.0) * 20.0,
    }
}

fn measure_ttf_text(text: &str, font_size_px: f32, font: &Font<'static>) -> u32 {
    if text.is_empty() {
        return 0;
    }

    let scale = Scale::uniform(font_size_px);
    let v_metrics = font.v_metrics(scale);
    let glyphs: Vec<_> = font
        .layout(text, scale, point(0.0, v_metrics.ascent))
        .collect();
    glyphs
        .iter()
        .rev()
        .find_map(|glyph| glyph.pixel_bounding_box().map(|bb| bb.max.x.max(0) as u32))
        .or_else(|| {
            glyphs.last().map(|glyph| {
                let end = glyph.position().x + glyph.unpositioned().h_metrics().advance_width;
                end.max(0.0).ceil() as u32
            })
        })
        .unwrap_or(0)
}

fn draw_ttf_text(
    canvas: &mut Canvas,
    text: &str,
    x: i32,
    y: i32,
    color: Color,
    font_size_px: f32,
    font: &Font<'static>,
) {
    let scale = Scale::uniform(font_size_px);
    let v_metrics = font.v_metrics(scale);
    let glyphs = font.layout(text, scale, point(x as f32, y as f32 + v_metrics.ascent));

    for glyph in glyphs {
        if let Some(bb) = glyph.pixel_bounding_box() {
            glyph.draw(|gx, gy, coverage| {
                let alpha = ((color[3] as f32) * coverage).round().clamp(0.0, 255.0) as u8;
                canvas.put_pixel_blend(
                    bb.min.x + gx as i32,
                    bb.min.y + gy as i32,
                    [color[0], color[1], color[2], alpha],
                );
            });
        }
    }
}

/// A canvas wrapper that clips all drawing operations to a sub-rectangle.
///
/// Created via [`Canvas::clipped()`]. All coordinates are in the parent
/// canvas's global coordinate space; the clip rectangle simply prevents
/// drawing outside its bounds.
pub struct ClippedCanvas<'a> {
    pub canvas: &'a mut Canvas,
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

impl<'a> ClippedCanvas<'a> {
    pub fn new(canvas: &'a mut Canvas, x: i32, y: i32, w: i32, h: i32) -> Self {
        Self {
            x0: x.max(0),
            y0: y.max(0),
            x1: (x + w).min(canvas.w as i32),
            y1: (y + h).min(canvas.h as i32),
            canvas,
        }
    }

    /// Clip bounds as `(x0, y0, x1, y1)`.
    pub fn bounds(&self) -> (i32, i32, i32, i32) {
        (self.x0, self.y0, self.x1, self.y1)
    }

    /// Width of the clip region.
    pub fn width(&self) -> i32 {
        self.x1 - self.x0
    }

    /// Height of the clip region.
    pub fn height(&self) -> i32 {
        self.y1 - self.y0
    }

    #[inline]
    pub fn put_pixel_blend(&mut self, x: i32, y: i32, col: Color) {
        if x >= self.x0 && x < self.x1 && y >= self.y0 && y < self.y1 {
            self.canvas.put_pixel_blend(x, y, col);
        }
    }

    pub fn draw_line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, col: Color) {
        let dx = (x1 - x0).abs();
        let dy = -(y1 - y0).abs();
        if dx > 10000 || dy.abs() > 10000 {
            return;
        }
        let sx: i32 = if x0 < x1 { 1 } else { -1 };
        let sy: i32 = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;
        let mut cx = x0;
        let mut cy = y0;
        let max_steps = dx.max(dy.abs()) + 1;
        let mut steps = 0;
        loop {
            self.put_pixel_blend(cx, cy, col);
            if cx == x1 && cy == y1 {
                break;
            }
            steps += 1;
            if steps > max_steps {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                cx += sx;
            }
            if e2 <= dx {
                err += dx;
                cy += sy;
            }
        }
    }

    pub fn draw_line_aa(&mut self, x0: f64, y0: f64, x1: f64, y1: f64, col: Color) {
        if !x0.is_finite() || !y0.is_finite() || !x1.is_finite() || !y1.is_finite() {
            return;
        }
        let steep = (y1 - y0).abs() > (x1 - x0).abs();
        let (mut x0, mut y0, mut x1, mut y1) = if steep {
            (y0, x0, y1, x1)
        } else {
            (x0, y0, x1, y1)
        };
        if x0 > x1 {
            std::mem::swap(&mut x0, &mut x1);
            std::mem::swap(&mut y0, &mut y1);
        }
        let dx = x1 - x0;
        let dy = y1 - y0;
        let gradient = if dx.abs() < 0.001 { 1.0 } else { dy / dx };
        let xpxl1 = x0.round() as i32;
        let yend = y0 + gradient * (x0.round() - x0);
        let mut intery = yend + gradient;
        let xpxl2 = x1.round() as i32;
        if (xpxl2 - xpxl1).abs() > 10000 {
            return;
        }
        for x in xpxl1..=xpxl2 {
            let fpart = intery - intery.floor();
            let y = intery as i32;
            let a1 = ((1.0 - fpart) * col[3] as f64) as u8;
            let a2 = (fpart * col[3] as f64) as u8;
            if steep {
                self.put_pixel_blend(y, x, [col[0], col[1], col[2], a1]);
                self.put_pixel_blend(y + 1, x, [col[0], col[1], col[2], a2]);
            } else {
                self.put_pixel_blend(x, y, [col[0], col[1], col[2], a1]);
                self.put_pixel_blend(x, y + 1, [col[0], col[1], col[2], a2]);
            }
            intery += gradient;
        }
    }

    pub fn draw_thick_line_aa(
        &mut self,
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
        col: Color,
        thickness: i32,
    ) {
        let dx = x1 - x0;
        let dy = y1 - y0;
        let len = (dx * dx + dy * dy).sqrt().max(1.0);
        let nx = -dy / len;
        let ny = dx / len;
        for d in -(thickness / 2)..=(thickness / 2) {
            let off = d as f64;
            self.draw_line_aa(
                x0 + nx * off,
                y0 + ny * off,
                x1 + nx * off,
                y1 + ny * off,
                col,
            );
        }
    }

    pub fn fill_span(&mut self, y: i32, x_left: i32, x_right: i32, col: Color) {
        if y < self.y0 || y >= self.y1 {
            return;
        }
        let l = x_left.max(self.x0);
        let r = x_right.min(self.x1 - 1);
        for x in l..=r {
            self.canvas.put_pixel_blend(x, y, col);
        }
    }

    pub fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, col: Color) {
        for row in y..y + h {
            self.fill_span(row, x, x + w - 1, col);
        }
    }

    pub fn draw_rect(&mut self, x: i32, y: i32, w: i32, h: i32, col: Color) {
        self.draw_line(x, y, x + w - 1, y, col);
        self.draw_line(x, y + h - 1, x + w - 1, y + h - 1, col);
        self.draw_line(x, y, x, y + h - 1, col);
        self.draw_line(x + w - 1, y, x + w - 1, y + h - 1, col);
    }

    pub fn draw_circle(&mut self, cx: i32, cy: i32, r: i32, col: Color) {
        let mut x = r;
        let mut y = 0;
        let mut err = 1 - r;
        while x >= y {
            self.put_pixel_blend(cx + x, cy + y, col);
            self.put_pixel_blend(cx - x, cy + y, col);
            self.put_pixel_blend(cx + x, cy - y, col);
            self.put_pixel_blend(cx - x, cy - y, col);
            self.put_pixel_blend(cx + y, cy + x, col);
            self.put_pixel_blend(cx - y, cy + x, col);
            self.put_pixel_blend(cx + y, cy - x, col);
            self.put_pixel_blend(cx - y, cy - x, col);
            y += 1;
            if err < 0 {
                err += 2 * y + 1;
            } else {
                x -= 1;
                err += 2 * (y - x) + 1;
            }
        }
    }

    pub fn draw_text(&mut self, text: &str, px: i32, py: i32, col: Color) {
        self.canvas.draw_text(text, px, py, col);
    }

    pub fn draw_text_right(&mut self, text: &str, right_x: i32, py: i32, col: Color) {
        let w = Canvas::text_width(text);
        self.draw_text(text, right_x - w, py, col);
    }
}

// ═══════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canvas_creation() {
        let c = Canvas::new(100, 50, [10, 20, 30, 255]);
        assert_eq!(c.pixels.len(), 100 * 50 * 4);
        assert_eq!(c.get_pixel(0, 0), [10, 20, 30, 255]);
        assert_eq!(c.get_pixel(99, 49), [10, 20, 30, 255]);
        assert_eq!(c.get_pixel(-1, 0), [0, 0, 0, 0]);
    }

    #[test]
    fn transparent_canvas() {
        let c = Canvas::new_transparent(10, 10);
        assert_eq!(c.get_pixel(5, 5), [0, 0, 0, 0]);
    }

    #[test]
    fn pixel_blend() {
        let mut c = Canvas::new(10, 10, [0, 0, 0, 255]);
        c.put_pixel_blend(5, 5, [255, 0, 0, 128]);
        let p = c.get_pixel(5, 5);
        assert!(p[0] > 100 && p[0] < 160);
        assert_eq!(p[1], 0);
        assert_eq!(p[2], 0);
        assert_eq!(p[3], 255);
    }

    #[test]
    fn pixel_blend_fast_paths() {
        let mut c = Canvas::new(10, 10, [100, 100, 100, 255]);
        // Fully opaque overwrites
        c.put_pixel_blend(0, 0, [255, 0, 0, 255]);
        assert_eq!(c.get_pixel(0, 0), [255, 0, 0, 255]);
        // Fully transparent is no-op
        c.put_pixel_blend(1, 1, [255, 0, 0, 0]);
        assert_eq!(c.get_pixel(1, 1), [100, 100, 100, 255]);
    }

    #[test]
    fn text_width_calculation() {
        assert_eq!(Canvas::text_width(""), 0);
        assert!(Canvas::text_width("A") > 0);
        assert!(Canvas::text_width("AB") > Canvas::text_width("A"));
    }

    #[test]
    fn clipped_canvas_bounds() {
        let mut c = Canvas::new(200, 200, [0, 0, 0, 255]);
        let clip = c.clipped(10, 20, 50, 60);
        assert_eq!(clip.bounds(), (10, 20, 60, 80));
        assert_eq!(clip.width(), 50);
        assert_eq!(clip.height(), 60);
    }

    #[test]
    fn clipped_drawing_respects_bounds() {
        let mut c = Canvas::new(100, 100, [0, 0, 0, 255]);
        {
            let mut clip = c.clipped(10, 10, 20, 20);
            // Inside clip region
            clip.put_pixel_blend(15, 15, [255, 0, 0, 255]);
            // Outside clip region -- should be ignored
            clip.put_pixel_blend(5, 5, [255, 0, 0, 255]);
        }
        assert_eq!(c.get_pixel(15, 15), [255, 0, 0, 255]);
        assert_eq!(c.get_pixel(5, 5), [0, 0, 0, 255]);
    }

    #[test]
    fn wind_barb_tail_points_upwind_in_screen_space() {
        let eps = 1e-12;

        // North wind: the wind vector points down-screen, but the barb staff
        // extends toward the source direction, up-screen.
        let (dx, dy) = Canvas::wind_barb_tail_unit_vector(0.0, 10.0, 10.0);
        assert!(dx.abs() < eps);
        assert!((dy + 1.0).abs() < eps);

        // West wind: the wind vector points right-screen, and the staff
        // extends left toward the source direction.
        let (dx, dy) = Canvas::wind_barb_tail_unit_vector(10.0, 0.0, 10.0);
        assert!((dx + 1.0).abs() < eps);
        assert!(dy.abs() < eps);
    }

    #[test]
    fn png_output() {
        let c = Canvas::new(4, 4, [255, 0, 0, 255]);
        let png = c.to_png();
        assert_eq!(&png[..4], &[0x89, b'P', b'N', b'G']);
    }

    #[test]
    fn fill_triangle_basic() {
        let mut c = Canvas::new(20, 20, [0, 0, 0, 255]);
        c.fill_triangle(10.0, 2.0, 2.0, 18.0, 18.0, 18.0, [255, 255, 255, 255]);
        // Center should be filled
        assert_eq!(c.get_pixel(10, 12), [255, 255, 255, 255]);
        // Corner should remain background
        assert_eq!(c.get_pixel(0, 0), [0, 0, 0, 255]);
    }

    #[test]
    fn fill_circle_basic() {
        let mut c = Canvas::new(30, 30, [0, 0, 0, 255]);
        c.fill_circle(15, 15, 5, [255, 0, 0, 255]);
        assert_eq!(c.get_pixel(15, 15), [255, 0, 0, 255]);
        assert_eq!(c.get_pixel(0, 0), [0, 0, 0, 255]);
    }
}
