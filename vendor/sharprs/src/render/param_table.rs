//! SHARPpy-style parameter table renderer.
//!
//! Renders the dense text table at the bottom of the SHARPpy sounding display
//! as an RGBA pixel buffer.  Uses a built-in 6x10 monospace bitmap font scaled
//! 2× so there are zero external dependencies (no image crate, no font files).
//!
//! # Layout (5 row groups)
//!
//! 1. **PARCELS** — SFC / ML / MU parcels (CAPE, CINH, LCL, LI, LFC, EL)
//! 2. **SHEAR / HELICITY** — layers (SFC-1km … Eff Shear)
//! 3. **INDICES** — PW, MeanW, RH, DCAPE, K-Index, TT, TEI, etc.
//! 4. **LAPSE RATES** — Sfc-3km, 3-6km, 850-500mb, 700-500mb
//! 5. **STORM MOTIONS** — Bunkers, Corfidi, STP, SCP, SHIP, etc.

// =========================================================================
// Bitmap font: 6x10 monospace (printable ASCII 0x20–0x7E)
// =========================================================================

/// Base character width in pixels (before scaling).
const CHAR_W: usize = 6;
/// Base character height in pixels (before scaling).
const CHAR_H: usize = 10;

/// Scale factor for rendering.  2× gives a 12×20 effective character.
const SCALE: usize = 2;

/// Scaled character width.
const SCALED_CW: usize = CHAR_W * SCALE;
/// Scaled character height.
const SCALED_CH: usize = CHAR_H * SCALE;
/// Line spacing (pixels between baselines) at current scale.
const LINE_H: usize = SCALED_CH + 4; // 24px

/// Minimal 6x10 bitmap font for printable ASCII 0x20..=0x7E.
///
/// Each glyph is 10 bytes (one per row, 6 MSBs used).  Generated from a
/// common public-domain 6x10 bitmap font.  Only the subset needed for the
/// parameter table is hand-encoded; missing glyphs render as a filled box.
fn glyph(ch: u8) -> [u8; CHAR_H] {
    // Returns 10 rows; each row is a byte where the top 6 bits are pixels.
    match ch {
        // Space
        b' ' => [0x00; 10],
        // Digits
        b'0' => [0x00, 0x38, 0x44, 0x4C, 0x54, 0x64, 0x44, 0x38, 0x00, 0x00],
        b'1' => [0x00, 0x10, 0x30, 0x10, 0x10, 0x10, 0x10, 0x38, 0x00, 0x00],
        b'2' => [0x00, 0x38, 0x44, 0x04, 0x18, 0x20, 0x40, 0x7C, 0x00, 0x00],
        b'3' => [0x00, 0x7C, 0x08, 0x10, 0x08, 0x04, 0x44, 0x38, 0x00, 0x00],
        b'4' => [0x00, 0x08, 0x18, 0x28, 0x48, 0x7C, 0x08, 0x08, 0x00, 0x00],
        b'5' => [0x00, 0x7C, 0x40, 0x78, 0x04, 0x04, 0x44, 0x38, 0x00, 0x00],
        b'6' => [0x00, 0x18, 0x20, 0x40, 0x78, 0x44, 0x44, 0x38, 0x00, 0x00],
        b'7' => [0x00, 0x7C, 0x04, 0x08, 0x10, 0x20, 0x20, 0x20, 0x00, 0x00],
        b'8' => [0x00, 0x38, 0x44, 0x44, 0x38, 0x44, 0x44, 0x38, 0x00, 0x00],
        b'9' => [0x00, 0x38, 0x44, 0x44, 0x3C, 0x04, 0x08, 0x30, 0x00, 0x00],
        // Upper-case letters
        b'A' => [0x00, 0x10, 0x28, 0x44, 0x44, 0x7C, 0x44, 0x44, 0x00, 0x00],
        b'B' => [0x00, 0x78, 0x44, 0x44, 0x78, 0x44, 0x44, 0x78, 0x00, 0x00],
        b'C' => [0x00, 0x38, 0x44, 0x40, 0x40, 0x40, 0x44, 0x38, 0x00, 0x00],
        b'D' => [0x00, 0x78, 0x44, 0x44, 0x44, 0x44, 0x44, 0x78, 0x00, 0x00],
        b'E' => [0x00, 0x7C, 0x40, 0x40, 0x78, 0x40, 0x40, 0x7C, 0x00, 0x00],
        b'F' => [0x00, 0x7C, 0x40, 0x40, 0x78, 0x40, 0x40, 0x40, 0x00, 0x00],
        b'G' => [0x00, 0x38, 0x44, 0x40, 0x5C, 0x44, 0x44, 0x3C, 0x00, 0x00],
        b'H' => [0x00, 0x44, 0x44, 0x44, 0x7C, 0x44, 0x44, 0x44, 0x00, 0x00],
        b'I' => [0x00, 0x38, 0x10, 0x10, 0x10, 0x10, 0x10, 0x38, 0x00, 0x00],
        b'J' => [0x00, 0x04, 0x04, 0x04, 0x04, 0x04, 0x44, 0x38, 0x00, 0x00],
        b'K' => [0x00, 0x44, 0x48, 0x50, 0x60, 0x50, 0x48, 0x44, 0x00, 0x00],
        b'L' => [0x00, 0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x7C, 0x00, 0x00],
        b'M' => [0x00, 0x44, 0x6C, 0x54, 0x54, 0x44, 0x44, 0x44, 0x00, 0x00],
        b'N' => [0x00, 0x44, 0x64, 0x54, 0x4C, 0x44, 0x44, 0x44, 0x00, 0x00],
        b'O' => [0x00, 0x38, 0x44, 0x44, 0x44, 0x44, 0x44, 0x38, 0x00, 0x00],
        b'P' => [0x00, 0x78, 0x44, 0x44, 0x78, 0x40, 0x40, 0x40, 0x00, 0x00],
        b'Q' => [0x00, 0x38, 0x44, 0x44, 0x44, 0x54, 0x48, 0x34, 0x00, 0x00],
        b'R' => [0x00, 0x78, 0x44, 0x44, 0x78, 0x50, 0x48, 0x44, 0x00, 0x00],
        b'S' => [0x00, 0x38, 0x44, 0x40, 0x38, 0x04, 0x44, 0x38, 0x00, 0x00],
        b'T' => [0x00, 0x7C, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x00, 0x00],
        b'U' => [0x00, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x38, 0x00, 0x00],
        b'V' => [0x00, 0x44, 0x44, 0x44, 0x44, 0x28, 0x28, 0x10, 0x00, 0x00],
        b'W' => [0x00, 0x44, 0x44, 0x44, 0x54, 0x54, 0x6C, 0x44, 0x00, 0x00],
        b'X' => [0x00, 0x44, 0x44, 0x28, 0x10, 0x28, 0x44, 0x44, 0x00, 0x00],
        b'Y' => [0x00, 0x44, 0x44, 0x28, 0x10, 0x10, 0x10, 0x10, 0x00, 0x00],
        b'Z' => [0x00, 0x7C, 0x04, 0x08, 0x10, 0x20, 0x40, 0x7C, 0x00, 0x00],
        // Lower-case letters
        b'a' => [0x00, 0x00, 0x00, 0x38, 0x04, 0x3C, 0x44, 0x3C, 0x00, 0x00],
        b'b' => [0x00, 0x40, 0x40, 0x78, 0x44, 0x44, 0x44, 0x78, 0x00, 0x00],
        b'c' => [0x00, 0x00, 0x00, 0x38, 0x44, 0x40, 0x44, 0x38, 0x00, 0x00],
        b'd' => [0x00, 0x04, 0x04, 0x3C, 0x44, 0x44, 0x44, 0x3C, 0x00, 0x00],
        b'e' => [0x00, 0x00, 0x00, 0x38, 0x44, 0x7C, 0x40, 0x38, 0x00, 0x00],
        b'f' => [0x00, 0x18, 0x24, 0x20, 0x78, 0x20, 0x20, 0x20, 0x00, 0x00],
        b'g' => [0x00, 0x00, 0x00, 0x3C, 0x44, 0x44, 0x3C, 0x04, 0x38, 0x00],
        b'h' => [0x00, 0x40, 0x40, 0x78, 0x44, 0x44, 0x44, 0x44, 0x00, 0x00],
        b'i' => [0x00, 0x10, 0x00, 0x30, 0x10, 0x10, 0x10, 0x38, 0x00, 0x00],
        b'j' => [0x00, 0x08, 0x00, 0x18, 0x08, 0x08, 0x08, 0x48, 0x30, 0x00],
        b'k' => [0x00, 0x40, 0x40, 0x48, 0x50, 0x60, 0x50, 0x48, 0x00, 0x00],
        b'l' => [0x00, 0x30, 0x10, 0x10, 0x10, 0x10, 0x10, 0x38, 0x00, 0x00],
        b'm' => [0x00, 0x00, 0x00, 0x68, 0x54, 0x54, 0x54, 0x44, 0x00, 0x00],
        b'n' => [0x00, 0x00, 0x00, 0x78, 0x44, 0x44, 0x44, 0x44, 0x00, 0x00],
        b'o' => [0x00, 0x00, 0x00, 0x38, 0x44, 0x44, 0x44, 0x38, 0x00, 0x00],
        b'p' => [0x00, 0x00, 0x00, 0x78, 0x44, 0x44, 0x78, 0x40, 0x40, 0x00],
        b'q' => [0x00, 0x00, 0x00, 0x3C, 0x44, 0x44, 0x3C, 0x04, 0x04, 0x00],
        b'r' => [0x00, 0x00, 0x00, 0x58, 0x64, 0x40, 0x40, 0x40, 0x00, 0x00],
        b's' => [0x00, 0x00, 0x00, 0x3C, 0x40, 0x38, 0x04, 0x78, 0x00, 0x00],
        b't' => [0x00, 0x20, 0x20, 0x78, 0x20, 0x20, 0x24, 0x18, 0x00, 0x00],
        b'u' => [0x00, 0x00, 0x00, 0x44, 0x44, 0x44, 0x44, 0x3C, 0x00, 0x00],
        b'v' => [0x00, 0x00, 0x00, 0x44, 0x44, 0x44, 0x28, 0x10, 0x00, 0x00],
        b'w' => [0x00, 0x00, 0x00, 0x44, 0x44, 0x54, 0x54, 0x28, 0x00, 0x00],
        b'x' => [0x00, 0x00, 0x00, 0x44, 0x28, 0x10, 0x28, 0x44, 0x00, 0x00],
        b'y' => [0x00, 0x00, 0x00, 0x44, 0x44, 0x3C, 0x04, 0x04, 0x38, 0x00],
        b'z' => [0x00, 0x00, 0x00, 0x7C, 0x08, 0x10, 0x20, 0x7C, 0x00, 0x00],
        // Punctuation / symbols used in the table
        b'.' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00],
        b',' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x10, 0x20, 0x00],
        b'-' => [0x00, 0x00, 0x00, 0x00, 0x7C, 0x00, 0x00, 0x00, 0x00, 0x00],
        b'+' => [0x00, 0x00, 0x00, 0x10, 0x10, 0x7C, 0x10, 0x10, 0x00, 0x00],
        b'/' => [0x00, 0x04, 0x04, 0x08, 0x10, 0x20, 0x40, 0x40, 0x00, 0x00],
        b'(' => [0x00, 0x08, 0x10, 0x20, 0x20, 0x20, 0x10, 0x08, 0x00, 0x00],
        b')' => [0x00, 0x20, 0x10, 0x08, 0x08, 0x08, 0x10, 0x20, 0x00, 0x00],
        b'%' => [0x00, 0x44, 0x48, 0x08, 0x10, 0x20, 0x24, 0x44, 0x00, 0x00],
        b':' => [0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00],
        b'|' => [0x00, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x00, 0x00],
        b'*' => [0x00, 0x00, 0x44, 0x28, 0x7C, 0x28, 0x44, 0x00, 0x00, 0x00],
        b'=' => [0x00, 0x00, 0x00, 0x7C, 0x00, 0x7C, 0x00, 0x00, 0x00, 0x00],
        b'_' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x7C, 0x00],
        b'~' => [0x00, 0x00, 0x00, 0x24, 0x58, 0x00, 0x00, 0x00, 0x00, 0x00],
        b'&' => [0x00, 0x30, 0x48, 0x30, 0x50, 0x4C, 0x44, 0x3A, 0x00, 0x00],
        b'^' => [0x00, 0x10, 0x28, 0x44, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
        b'>' => [0x00, 0x40, 0x20, 0x10, 0x08, 0x10, 0x20, 0x40, 0x00, 0x00],
        b'<' => [0x00, 0x04, 0x08, 0x10, 0x20, 0x10, 0x08, 0x04, 0x00, 0x00],
        b'[' => [0x00, 0x38, 0x20, 0x20, 0x20, 0x20, 0x20, 0x38, 0x00, 0x00],
        b']' => [0x00, 0x38, 0x08, 0x08, 0x08, 0x08, 0x08, 0x38, 0x00, 0x00],
        b'#' => [0x00, 0x28, 0x28, 0x7C, 0x28, 0x7C, 0x28, 0x28, 0x00, 0x00],
        b'\xb0' | b'@' => [0x10, 0x28, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // degree symbol (use @ as alias)
        // Default: filled rectangle (unknown glyph)
        _ => [0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C],
    }
}

// =========================================================================
// Colour constants (RGBA)
// =========================================================================

/// RGBA colour type: [R, G, B, A].
pub type Rgba = [u8; 4];

pub const BLACK: Rgba = [0, 0, 0, 255];
pub const WHITE: Rgba = [255, 255, 255, 255];
pub const CYAN: Rgba = [0, 255, 255, 255];
pub const YELLOW: Rgba = [255, 255, 0, 255];
pub const GREEN: Rgba = [0, 255, 0, 255];
pub const RED: Rgba = [255, 80, 80, 255];
pub const ORANGE: Rgba = [255, 165, 0, 255];
/// Dim gray for separator lines.
pub const DIM_GRAY: Rgba = [80, 80, 80, 255];
/// Slightly brighter gray for section separator lines.
pub const MED_GRAY: Rgba = [110, 110, 110, 255];

// =========================================================================
// Pixel buffer helpers
// =========================================================================

/// A rectangular RGBA pixel buffer.
pub struct PixelBuf {
    pub width: usize,
    pub height: usize,
    /// Row-major RGBA pixels.  Length = width * height * 4.
    pub data: Vec<u8>,
}

impl PixelBuf {
    /// Create a new buffer filled with `bg`.
    pub fn new(width: usize, height: usize, bg: Rgba) -> Self {
        let mut data = vec![0u8; width * height * 4];
        for pixel in data.chunks_exact_mut(4) {
            pixel.copy_from_slice(&bg);
        }
        PixelBuf {
            width,
            height,
            data,
        }
    }

    /// Set a single pixel.
    #[inline]
    pub fn set(&mut self, x: usize, y: usize, colour: Rgba) {
        if x < self.width && y < self.height {
            let off = (y * self.width + x) * 4;
            self.data[off..off + 4].copy_from_slice(&colour);
        }
    }

    /// Draw a horizontal line from (x0,y) to (x1,y).
    pub fn hline(&mut self, x0: usize, x1: usize, y: usize, colour: Rgba) {
        let start = x0.min(x1);
        let end = x1.max(x0);
        for x in start..=end.min(self.width.saturating_sub(1)) {
            self.set(x, y, colour);
        }
    }

    /// Draw a thicker horizontal line (2px tall) for section separators.
    pub fn hline_thick(&mut self, x0: usize, x1: usize, y: usize, colour: Rgba) {
        self.hline(x0, x1, y, colour);
        self.hline(x0, x1, y + 1, colour);
    }

    /// Draw a single character at (px, py) in the given colour, scaled by SCALE.
    fn draw_char_scaled(&mut self, ch: u8, px: usize, py: usize, colour: Rgba) {
        let g = glyph(ch);
        for (row, &bits) in g.iter().enumerate() {
            for col in 0..CHAR_W {
                if bits & (0x80 >> col) != 0 {
                    // Draw a SCALE×SCALE block for each set pixel
                    for sy in 0..SCALE {
                        for sx in 0..SCALE {
                            let x = px + col * SCALE + sx;
                            let y = py + row * SCALE + sy;
                            self.set(x, y, colour);
                        }
                    }
                }
            }
        }
    }

    /// Draw a string at (px, py) in the given colour (scaled).
    /// Returns the X coordinate after the last character.
    pub fn draw_str(&mut self, s: &str, px: usize, py: usize, colour: Rgba) -> usize {
        let mut cx = px;
        for &b in s.as_bytes() {
            self.draw_char_scaled(b, cx, py, colour);
            cx += SCALED_CW;
        }
        cx
    }

    /// Draw a string right-aligned so that it ends at `right_x`.
    pub fn draw_str_right(&mut self, s: &str, right_x: usize, py: usize, colour: Rgba) {
        let w = s.len() * SCALED_CW;
        let px = right_x.saturating_sub(w);
        self.draw_str(s, px, py, colour);
    }
}

// =========================================================================
// Formatting helpers
// =========================================================================

/// Format an f64 as an integer, or "M" if NaN / missing.
fn fmt_int(v: f64) -> String {
    if v.is_finite() && (v - crate::constants::MISSING).abs() > 1.0 {
        format!("{:.0}", v)
    } else {
        "M".to_string()
    }
}

/// Format an f64 to 1 decimal place, or "M" if missing.
fn fmt_1f(v: f64) -> String {
    if v.is_finite() && (v - crate::constants::MISSING).abs() > 1.0 {
        format!("{:.1}", v)
    } else {
        "M".to_string()
    }
}

/// Format an f64 to 2 decimal places, or "M" if missing.
fn fmt_2f(v: f64) -> String {
    if v.is_finite() && (v - crate::constants::MISSING).abs() > 1.0 {
        format!("{:.2}", v)
    } else {
        "M".to_string()
    }
}

/// Format a direction/speed pair as "ddd/ss", or "M" if missing.
fn fmt_dir_spd(direction: f64, speed: f64) -> String {
    if direction.is_finite()
        && speed.is_finite()
        && (direction - crate::constants::MISSING).abs() > 1.0
        && (speed - crate::constants::MISSING).abs() > 1.0
    {
        format!("{:.0}/{:.0}", direction, speed)
    } else {
        "M".to_string()
    }
}

/// Right-pad a string to a fixed width.
fn rpad(s: &str, w: usize) -> String {
    if s.len() >= w {
        s[..w].to_string()
    } else {
        format!("{:<width$}", s, width = w)
    }
}

/// Left-pad (right-align) a string to a fixed width.
fn lpad(s: &str, w: usize) -> String {
    if s.len() >= w {
        s[..w].to_string()
    } else {
        format!("{:>width$}", s, width = w)
    }
}

// =========================================================================
// Color-coding helpers
// =========================================================================

/// Choose color for CAPE values.
fn cape_color(v: f64) -> Rgba {
    if !v.is_finite() {
        return WHITE;
    }
    if v >= 4000.0 {
        RED
    } else if v >= 3000.0 {
        ORANGE
    } else if v >= 2000.0 {
        YELLOW
    } else {
        WHITE
    }
}

/// Choose color for STP values.
fn stp_color(v: f64) -> Rgba {
    if !v.is_finite() {
        return YELLOW;
    }
    if v >= 4.0 {
        RED
    } else if v >= 2.0 {
        ORANGE
    } else if v >= 1.0 {
        YELLOW
    } else {
        WHITE
    }
}

/// Choose color for SCP values.
fn scp_color(v: f64) -> Rgba {
    if !v.is_finite() {
        return WHITE;
    }
    if v >= 8.0 {
        RED
    } else if v >= 4.0 {
        ORANGE
    } else if v >= 1.0 {
        YELLOW
    } else {
        WHITE
    }
}

/// Choose color for SHIP values.
fn ship_color(v: f64) -> Rgba {
    if !v.is_finite() {
        return WHITE;
    }
    if v >= 2.0 {
        RED
    } else if v >= 1.0 {
        YELLOW
    } else {
        WHITE
    }
}

/// Choose color for lapse rate values (C/km) — steeper = more dangerous.
fn lapse_color(v: f64) -> Rgba {
    if !v.is_finite() {
        return WHITE;
    }
    if v >= 9.0 {
        RED
    } else if v >= 8.0 {
        ORANGE
    } else if v >= 7.0 {
        YELLOW
    } else {
        WHITE
    }
}

/// Choose color for SRH values (m2/s2).
fn srh_color(v: f64) -> Rgba {
    if !v.is_finite() {
        return WHITE;
    }
    if v >= 400.0 {
        RED
    } else if v >= 200.0 {
        ORANGE
    } else if v >= 100.0 {
        YELLOW
    } else {
        WHITE
    }
}

/// Choose color for shear magnitude (kt).
fn shear_color(v: f64) -> Rgba {
    if !v.is_finite() {
        return WHITE;
    }
    if v >= 60.0 {
        RED
    } else if v >= 40.0 {
        YELLOW
    } else {
        WHITE
    }
}

// =========================================================================
// ParamTableData — all pre-computed parameters for the table
// =========================================================================

/// Pre-computed parcel results for one parcel type.
#[derive(Debug, Clone, Default)]
pub struct ParcelRow {
    /// Label: "SFC", "ML", "FCST", "MU"
    pub label: String,
    /// Entraining CAPE (J/kg), when supplied by an ECAPE bridge.
    pub ecape: f64,
    /// Normalized CAPE (J/kg), when supplied by an ECAPE bridge.
    pub ncape: f64,
    /// CAPE (J/kg).
    pub cape: f64,
    /// CAPE in the lowest 3 km AGL (J/kg).
    pub cape_3km: f64,
    /// CAPE in the lowest 6 km AGL (J/kg).
    pub cape_6km: f64,
    /// CIN (J/kg, negative).
    pub cinh: f64,
    /// LCL height (m AGL).
    pub lcl_m: f64,
    /// Lifted Index (500 hPa, C).
    pub li: f64,
    /// LFC height (m AGL).
    pub lfc_m: f64,
    /// EL height (m AGL).
    pub el_m: f64,
}

/// Shear/helicity data for one layer.
#[derive(Debug, Clone, Default)]
pub struct ShearRow {
    /// Layer label, e.g. "SFC-1km"
    pub label: String,
    /// Energy-Helicity Index.
    pub ehi: f64,
    /// Storm-Relative Helicity (m^2/s^2).
    pub srh: f64,
    /// Shear magnitude (kt).
    pub shear: f64,
    /// Mean wind speed (kt).
    pub mn_wind: f64,
    /// Storm-relative wind direction (degrees).
    pub srw_dir: f64,
    /// Storm-relative wind speed (kt).
    pub srw_spd: f64,
    /// Storm-relative wind speed (kt).
    pub srw: f64,
}

/// Storm motion vector (direction/speed).
#[derive(Debug, Clone, Default)]
pub struct StormMotion {
    pub label: String,
    /// Direction (degrees).
    pub direction: f64,
    /// Speed (knots).
    pub speed: f64,
}

/// Lapse rate for a given layer.
#[derive(Debug, Clone, Default)]
pub struct LapseRateRow {
    pub label: String,
    /// Lapse rate (C/km).
    pub value: f64,
}

/// All parameters needed to render the full SHARPpy bottom-panel table.
#[derive(Debug, Clone, Default)]
pub struct ParamTableData {
    // Row 1 — Parcel data (up to 4 parcels)
    pub parcels: Vec<ParcelRow>,

    // Row 2 — Shear / helicity (up to 7 layers)
    pub shear_layers: Vec<ShearRow>,

    // Row 3 — Thermodynamic indices
    pub pw: f64,               // Precipitable water (in)
    pub mean_w: f64,           // Mean mixing ratio (g/kg)
    pub sfc_rh: f64,           // Surface RH (%)
    pub low_rh: f64,           // Low-level mean RH (%)
    pub mid_rh: f64,           // Mid-level mean RH (%)
    pub dgz_rh: f64,           // Mean RH in the dendritic growth zone (%)
    pub freezing_level_m: f64, // Freezing level (m AGL)
    pub wb_zero_m: f64,        // Wet-bulb zero height (m AGL)
    pub mu_mpl_m: f64,         // MU maximum parcel level (m AGL)
    pub thetae_diff_3km: f64,  // 3 km theta-e difference (K)
    pub lcl_temp_c: f64,       // Surface parcel LCL temperature (C)
    pub dcape: f64,            // Downdraft CAPE (J/kg)
    pub dwn_t: f64,            // Downdraft temperature (F or C)
    pub k_index: f64,          // K-Index
    pub t_totals: f64,         // Total Totals
    pub tei: f64,              // Theta-E Index (K)
    pub tehi: f64,             // Tornadic 0-1 km EHI
    pub tts: f64,              // Tornadic Tilting and Stretching
    pub vtp_mod: f64,          // Modified Violent Tornado Parameter
    pub conv_t: f64,           // Convective temperature (F or C)
    pub max_t: f64,            // Forecast max temperature (F or C)
    pub mmp: f64,              // MCS Maintenance Probability
    pub sig_svr: f64,          // Significant Severe (m^3/s^3)
    pub esp: f64,              // Enhanced Stretching Potential
    pub wndg: f64,             // Wind Damage Parameter
    pub dcp: f64,              // Derecho Composite Parameter
    pub lhp: f64,              // Large Hail Parameter
    pub cape_3km: f64,         // 0-3 km CAPE (J/kg)

    // Row 4 — Lapse rates
    pub lapse_rates: Vec<LapseRateRow>,

    // Row 5 — Storm motions
    pub bunkers_right: StormMotion,
    pub bunkers_left: StormMotion,
    pub corfidi_down: StormMotion,
    pub corfidi_up: StormMotion,

    // Row 5 — Composite parameters
    pub stp_cin: f64,   // STP (CIN-based)
    pub stp_fix: f64,   // STP (fixed-layer)
    pub ship: f64,      // Significant Hail Parameter
    pub scp: f64,       // Supercell Composite
    pub brn_shear: f64, // BRN Shear (m^2/s^2)

    // 1km and 6km AGL wind (direction/speed) for barb labels
    pub wind_1km_dir: f64,
    pub wind_1km_spd: f64,
    pub wind_6km_dir: f64,
    pub wind_6km_spd: f64,
}

// =========================================================================
// Table renderer
// =========================================================================

/// Default table width (pixels).  Full width of 2400px composite.
pub const TABLE_WIDTH: usize = 2400;
/// Default table height (pixels).  ~636px bottom panel.
pub const TABLE_HEIGHT: usize = 636;

/// Left margin (pixels).
const LM: usize = 16;

/// Render the parameter table to a new `PixelBuf`.
///
/// The buffer is `TABLE_WIDTH` x `TABLE_HEIGHT` pixels, black background,
/// with CYAN headers, WHITE values, and colour-coded severe parameters.
pub fn render(data: &ParamTableData) -> PixelBuf {
    render_sized(data, TABLE_WIDTH, TABLE_HEIGHT)
}

/// Render the parameter table into a buffer of the given dimensions.
pub fn render_sized(data: &ParamTableData, width: usize, height: usize) -> PixelBuf {
    let mut buf = PixelBuf::new(width, height, BLACK);

    let mut y: usize = 6;

    // Column positions for the three-panel layout
    // Left panel: parcels / storm motions  (~col 0-780)
    // Middle panel: shear/helicity         (~col 800-1580)
    // Right panel: indices / lapse / composites (~col 1600-2380)
    let panel_left = LM;
    let panel_mid = width * 33 / 100; // ~792
    let panel_right = width * 66 / 100; // ~1584

    // =====================================================================
    // LEFT PANEL: PARCELS section
    // =====================================================================
    {
        // Section header
        buf.draw_str("PARCELS", panel_left, y, CYAN);
        let hdr_y = y + LINE_H;
        buf.hline_thick(panel_left, panel_mid - 20, hdr_y, MED_GRAY);
        y = hdr_y + 4;

        // Column headers with units
        let c0 = panel_left; // PCL label
        let c1 = panel_left + SCALED_CW * 8; // ECAPE
        let c2 = panel_left + SCALED_CW * 14; // NCAPE
        let c3 = panel_left + SCALED_CW * 20; // CAPE
        let c4 = panel_left + SCALED_CW * 26; // 3CAPE
        let c5 = panel_left + SCALED_CW * 32; // 6CAPE
        let c6 = panel_left + SCALED_CW * 38; // CINH
        let c7 = panel_left + SCALED_CW * 44; // LCL
        let c8 = panel_left + SCALED_CW * 51; // LFC
        let c9 = panel_left + SCALED_CW * 58; // EL

        buf.draw_str("PCL", c0, y, CYAN);
        buf.draw_str_right("ECAPE", c1 + SCALED_CW * 5, y, CYAN);
        buf.draw_str_right("NCAPE", c2 + SCALED_CW * 5, y, CYAN);
        buf.draw_str_right("CAPE", c3 + SCALED_CW * 5, y, CYAN);
        buf.draw_str_right("3CAPE", c4 + SCALED_CW * 5, y, CYAN);
        buf.draw_str_right("6CAPE", c5 + SCALED_CW * 5, y, CYAN);
        buf.draw_str_right("CINH", c6 + SCALED_CW * 5, y, CYAN);
        buf.draw_str_right("LCL", c7 + SCALED_CW * 5, y, CYAN);
        buf.draw_str_right("LFC", c8 + SCALED_CW * 5, y, CYAN);
        buf.draw_str_right("EL", c9 + SCALED_CW * 5, y, CYAN);
        y += LINE_H;

        // Units sub-header
        buf.draw_str_right("J/kg", c1 + SCALED_CW * 5, y, DIM_GRAY);
        buf.draw_str_right("J/kg", c2 + SCALED_CW * 5, y, DIM_GRAY);
        buf.draw_str_right("J/kg", c3 + SCALED_CW * 5, y, DIM_GRAY);
        buf.draw_str_right("J/kg", c4 + SCALED_CW * 5, y, DIM_GRAY);
        buf.draw_str_right("J/kg", c5 + SCALED_CW * 5, y, DIM_GRAY);
        buf.draw_str_right("J/kg", c6 + SCALED_CW * 5, y, DIM_GRAY);
        buf.draw_str_right("m", c7 + SCALED_CW * 5, y, DIM_GRAY);
        buf.draw_str_right("m", c8 + SCALED_CW * 5, y, DIM_GRAY);
        buf.draw_str_right("m", c9 + SCALED_CW * 5, y, DIM_GRAY);
        y += LINE_H;

        // Thin separator
        buf.hline(panel_left, panel_mid - 20, y, DIM_GRAY);
        y += 3;

        for pcl in &data.parcels {
            buf.draw_str(&rpad(&pcl.label, 7), c0, y, WHITE);
            buf.draw_str_right(&lpad(&fmt_int(pcl.ecape), 5), c1 + SCALED_CW * 5, y, WHITE);
            buf.draw_str_right(&lpad(&fmt_1f(pcl.ncape), 5), c2 + SCALED_CW * 5, y, WHITE);
            let cape_s = fmt_int(pcl.cape);
            buf.draw_str_right(
                &lpad(&cape_s, 5),
                c3 + SCALED_CW * 5,
                y,
                cape_color(pcl.cape),
            );
            buf.draw_str_right(
                &lpad(&fmt_int(pcl.cape_3km), 5),
                c4 + SCALED_CW * 5,
                y,
                WHITE,
            );
            buf.draw_str_right(
                &lpad(&fmt_int(pcl.cape_6km), 5),
                c5 + SCALED_CW * 5,
                y,
                WHITE,
            );
            buf.draw_str_right(&lpad(&fmt_int(pcl.cinh), 5), c6 + SCALED_CW * 5, y, WHITE);
            buf.draw_str_right(&lpad(&fmt_int(pcl.lcl_m), 5), c7 + SCALED_CW * 5, y, WHITE);
            buf.draw_str_right(&lpad(&fmt_int(pcl.lfc_m), 5), c8 + SCALED_CW * 5, y, WHITE);
            buf.draw_str_right(&lpad(&fmt_int(pcl.el_m), 5), c9 + SCALED_CW * 5, y, WHITE);
            y += LINE_H;
        }
    }

    // =====================================================================
    // MIDDLE PANEL: SHEAR / HELICITY section
    // =====================================================================
    let shear_start_y;
    {
        let mut sy: usize = 6;
        buf.draw_str("SHEAR / HELICITY", panel_mid, sy, CYAN);
        let hdr_y = sy + LINE_H;
        buf.hline_thick(panel_mid, panel_right - 20, hdr_y, MED_GRAY);
        sy = hdr_y + 4;

        // Column headers with units
        let s0 = panel_mid; // Layer
        let s1 = panel_mid + SCALED_CW * 14; // EHI
        let s2 = panel_mid + SCALED_CW * 20; // SRH
        let s3 = panel_mid + SCALED_CW * 26; // Shear
        let s4 = panel_mid + SCALED_CW * 32; // MnWind
        let s5 = panel_mid + SCALED_CW * 38; // SRWind

        buf.draw_str("Layer", s0, sy, CYAN);
        buf.draw_str_right("EHI", s1 + SCALED_CW * 5, sy, CYAN);
        buf.draw_str_right("SRH", s2 + SCALED_CW * 5, sy, CYAN);
        buf.draw_str_right("Shear", s3 + SCALED_CW * 5, sy, CYAN);
        buf.draw_str_right("MnWnd", s4 + SCALED_CW * 5, sy, CYAN);
        buf.draw_str_right("SRWind", s5 + SCALED_CW * 7, sy, CYAN);
        sy += LINE_H;

        // Units
        buf.draw_str_right("", s1 + SCALED_CW * 5, sy, DIM_GRAY);
        buf.draw_str_right("m2/s2", s2 + SCALED_CW * 5, sy, DIM_GRAY);
        buf.draw_str_right("kts", s3 + SCALED_CW * 5, sy, DIM_GRAY);
        buf.draw_str_right("kts", s4 + SCALED_CW * 5, sy, DIM_GRAY);
        buf.draw_str_right("deg/kt", s5 + SCALED_CW * 7, sy, DIM_GRAY);
        sy += LINE_H;

        buf.hline(panel_mid, panel_right - 20, sy, DIM_GRAY);
        sy += 3;

        for row in &data.shear_layers {
            buf.draw_str(&rpad(&row.label, 13), s0, sy, WHITE);
            buf.draw_str_right(&lpad(&fmt_1f(row.ehi), 5), s1 + SCALED_CW * 5, sy, WHITE);
            let srh_s = fmt_int(row.srh);
            buf.draw_str_right(&lpad(&srh_s, 5), s2 + SCALED_CW * 5, sy, srh_color(row.srh));
            let shear_s = fmt_int(row.shear);
            buf.draw_str_right(
                &lpad(&shear_s, 5),
                s3 + SCALED_CW * 5,
                sy,
                shear_color(row.shear),
            );
            buf.draw_str_right(
                &lpad(&fmt_int(row.mn_wind), 5),
                s4 + SCALED_CW * 5,
                sy,
                WHITE,
            );
            buf.draw_str_right(
                &lpad(&fmt_dir_spd(row.srw_dir, row.srw_spd), 7),
                s5 + SCALED_CW * 7,
                sy,
                WHITE,
            );
            sy += LINE_H;
        }

        shear_start_y = sy;
    }

    // =====================================================================
    // RIGHT PANEL: INDICES section
    // =====================================================================
    let indices_end_y;
    {
        let mut iy: usize = 6;
        buf.draw_str("INDICES", panel_right, iy, CYAN);
        let hdr_y = iy + LINE_H;
        buf.hline_thick(panel_right, width - LM, hdr_y, MED_GRAY);
        iy = hdr_y + 4;

        // Two sub-columns within the right panel
        let r0 = panel_right; // left label
        let r1 = panel_right + SCALED_CW * 9; // left value (right-aligned)
        let r2 = panel_right + SCALED_CW * 18; // right label
        let r3 = panel_right + SCALED_CW * 27; // right value (right-aligned)

        // Row by row — all indices
        let left_items: Vec<(&str, String, Rgba)> = vec![
            ("PW (in)", fmt_2f(data.pw), WHITE),
            ("MeanW g/kg", fmt_1f(data.mean_w), WHITE),
            ("SfcRH %", fmt_int(data.sfc_rh), WHITE),
            ("LowRH %", fmt_int(data.low_rh), WHITE),
            ("MidRH %", fmt_int(data.mid_rh), WHITE),
            ("DGZRH %", fmt_int(data.dgz_rh), WHITE),
            ("FrzLvl m", fmt_int(data.freezing_level_m), WHITE),
            ("WBZ m", fmt_int(data.wb_zero_m), WHITE),
            ("MU MPL m", fmt_int(data.mu_mpl_m), WHITE),
            ("3km Theta", fmt_int(data.thetae_diff_3km), WHITE),
            ("LCL Tmp C", fmt_1f(data.lcl_temp_c), WHITE),
            ("0-3km CAPE", fmt_int(data.cape_3km), WHITE),
        ];
        let right_items: Vec<(&str, String, Rgba)> = vec![
            ("K-Index", fmt_1f(data.k_index), WHITE),
            ("TotTots", fmt_1f(data.t_totals), WHITE),
            ("TEI", fmt_1f(data.tei), WHITE),
            ("TEHI", fmt_1f(data.tehi), WHITE),
            ("TTS", fmt_1f(data.tts), WHITE),
            ("ConvT", fmt_1f(data.conv_t), WHITE),
            ("MaxT", fmt_1f(data.max_t), WHITE),
            ("DCAPE", fmt_int(data.dcape), WHITE),
            ("DwnT", fmt_1f(data.dwn_t), WHITE),
            ("MMP", fmt_2f(data.mmp), WHITE),
            ("SigSvr", fmt_int(data.sig_svr), YELLOW),
            ("ESP", fmt_1f(data.esp), WHITE),
            ("WNDG", fmt_1f(data.wndg), WHITE),
        ];

        let rows = left_items.len().max(right_items.len());
        for i in 0..rows {
            if i < left_items.len() {
                buf.draw_str(left_items[i].0, r0, iy, CYAN);
                buf.draw_str_right(
                    &lpad(&left_items[i].1, 7),
                    r1 + SCALED_CW * 7,
                    iy,
                    left_items[i].2,
                );
            }
            if i < right_items.len() {
                buf.draw_str(right_items[i].0, r2, iy, CYAN);
                buf.draw_str_right(
                    &lpad(&right_items[i].1, 7),
                    r3 + SCALED_CW * 7,
                    iy,
                    right_items[i].2,
                );
            }
            iy += LINE_H;
        }

        indices_end_y = iy;
    }

    // =====================================================================
    // LEFT PANEL (lower): STORM MOTIONS section
    // =====================================================================
    let mut storm_y = y + 8; // continue below parcels
    {
        buf.draw_str("STORM MOTIONS", panel_left, storm_y, CYAN);
        storm_y += LINE_H;
        buf.hline_thick(panel_left, panel_mid - 20, storm_y, MED_GRAY);
        storm_y += 4;

        // Headers
        let mc0 = panel_left;
        let mc1 = panel_left + SCALED_CW * 18;
        let mc2 = panel_left + SCALED_CW * 24;

        buf.draw_str("Motion", mc0, storm_y, CYAN);
        buf.draw_str_right("Dir", mc1 + SCALED_CW * 4, storm_y, CYAN);
        buf.draw_str_right("Spd kts", mc2 + SCALED_CW * 7, storm_y, CYAN);
        storm_y += LINE_H;

        buf.hline(panel_left, panel_mid - 20, storm_y, DIM_GRAY);
        storm_y += 3;

        let motions = [
            ("Bunkers R", &data.bunkers_right),
            ("Bunkers L", &data.bunkers_left),
            ("Corfidi DN", &data.corfidi_down),
            ("Corfidi UP", &data.corfidi_up),
        ];
        for (name, m) in &motions {
            buf.draw_str(name, mc0, storm_y, WHITE);
            buf.draw_str_right(
                &lpad(&fmt_int(m.direction), 4),
                mc1 + SCALED_CW * 4,
                storm_y,
                WHITE,
            );
            buf.draw_str_right(
                &lpad(&fmt_int(m.speed), 4),
                mc2 + SCALED_CW * 7,
                storm_y,
                WHITE,
            );
            storm_y += LINE_H;
        }

        // Wind labels
        storm_y += 4;
        let wind_text = format!(
            "1km: {}/{} kt  6km: {}/{} kt",
            fmt_int(data.wind_1km_dir),
            fmt_int(data.wind_1km_spd),
            fmt_int(data.wind_6km_dir),
            fmt_int(data.wind_6km_spd),
        );
        buf.draw_str(&wind_text, panel_left, storm_y, GREEN);
    }

    // =====================================================================
    // MIDDLE PANEL (lower): LAPSE RATES section
    // =====================================================================
    {
        let mut ly = shear_start_y + 8;
        buf.draw_str("LAPSE RATES (C/km)", panel_mid, ly, CYAN);
        ly += LINE_H;
        buf.hline_thick(panel_mid, panel_right - 20, ly, MED_GRAY);
        ly += 4;

        let lr0 = panel_mid;
        let lr1 = panel_mid + SCALED_CW * 14;

        for lr in &data.lapse_rates {
            buf.draw_str(&rpad(&lr.label, 12), lr0, ly, WHITE);
            let val_s = fmt_1f(lr.value);
            buf.draw_str_right(
                &lpad(&val_s, 5),
                lr1 + SCALED_CW * 5,
                ly,
                lapse_color(lr.value),
            );
            ly += LINE_H;
        }
    }

    // =====================================================================
    // RIGHT PANEL (lower): COMPOSITES section
    // =====================================================================
    {
        let mut cy = indices_end_y + 8;
        buf.draw_str("COMPOSITES", panel_right, cy, CYAN);
        cy += LINE_H;
        buf.hline_thick(panel_right, width - LM, cy, MED_GRAY);
        cy += 4;

        let comp_items: Vec<(&str, String, Rgba)> = vec![
            ("STP(cin)", fmt_1f(data.stp_cin), stp_color(data.stp_cin)),
            ("STP(fix)", fmt_1f(data.stp_fix), stp_color(data.stp_fix)),
            ("SHIP", fmt_1f(data.ship), ship_color(data.ship)),
            ("Supercell", fmt_1f(data.scp), scp_color(data.scp)),
            ("VTP mod", fmt_1f(data.vtp_mod), stp_color(data.vtp_mod)),
            ("DCP", fmt_1f(data.dcp), WHITE),
            ("LHP", fmt_1f(data.lhp), WHITE),
            ("BRN Shear", fmt_1f(data.brn_shear), WHITE),
        ];

        let cc0 = panel_right;
        let cc1 = panel_right + SCALED_CW * 12;

        for (lbl, val, colour) in &comp_items {
            buf.draw_str(lbl, cc0, cy, CYAN);
            buf.draw_str_right(&lpad(val, 7), cc1 + SCALED_CW * 7, cy, *colour);
            cy += LINE_H;
        }
    }

    // =====================================================================
    // Vertical panel divider lines
    // =====================================================================
    for row in 0..height {
        buf.set(panel_mid.saturating_sub(10), row, DIM_GRAY);
        buf.set(panel_right.saturating_sub(10), row, DIM_GRAY);
    }

    buf
}

// =========================================================================
// Convenience: render into a caller-provided RGBA buffer (sub-region blit)
// =========================================================================

/// Blit the parameter table into an existing RGBA buffer at position (dx, dy).
///
/// `dst_width` is the stride (row width in pixels) of the destination buffer.
/// Out-of-bounds pixels are silently clipped.
pub fn blit_into(dst: &mut [u8], dst_width: usize, dx: usize, dy: usize, data: &ParamTableData) {
    let table = render(data);
    for row in 0..table.height {
        let ry = dy + row;
        if ry * dst_width * 4 >= dst.len() {
            break;
        }
        for col in 0..table.width {
            let rx = dx + col;
            let src_off = (row * table.width + col) * 4;
            let dst_off = (ry * dst_width + rx) * 4;
            if dst_off + 4 <= dst.len() {
                dst[dst_off..dst_off + 4].copy_from_slice(&table.data[src_off..src_off + 4]);
            }
        }
    }
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a sample `ParamTableData` with plausible values.
    fn sample_data() -> ParamTableData {
        ParamTableData {
            parcels: vec![
                ParcelRow {
                    label: "SFC".into(),
                    ecape: f64::NAN,
                    ncape: f64::NAN,
                    cape: 2500.0,
                    cape_3km: 120.0,
                    cape_6km: 900.0,
                    cinh: -45.0,
                    lcl_m: 1100.0,
                    li: -6.2,
                    lfc_m: 2100.0,
                    el_m: 12300.0,
                },
                ParcelRow {
                    label: "ML".into(),
                    ecape: f64::NAN,
                    ncape: f64::NAN,
                    cape: 1800.0,
                    cape_3km: 90.0,
                    cape_6km: 650.0,
                    cinh: -30.0,
                    lcl_m: 1300.0,
                    li: -4.8,
                    lfc_m: 2500.0,
                    el_m: 11800.0,
                },
                ParcelRow {
                    label: "MU".into(),
                    ecape: f64::NAN,
                    ncape: f64::NAN,
                    cape: 3100.0,
                    cape_3km: 150.0,
                    cape_6km: 1000.0,
                    cinh: -15.0,
                    lcl_m: 800.0,
                    li: -7.0,
                    lfc_m: 1500.0,
                    el_m: 12500.0,
                },
            ],
            shear_layers: vec![
                ShearRow {
                    label: "SFC-1km".into(),
                    ehi: 1.2,
                    srh: 180.0,
                    shear: 25.0,
                    mn_wind: 15.0,
                    srw_dir: 93.0,
                    srw_spd: 20.0,
                    srw: 12.0,
                },
                ShearRow {
                    label: "SFC-3km".into(),
                    ehi: 2.5,
                    srh: 280.0,
                    shear: 40.0,
                    mn_wind: 22.0,
                    srw_dir: 109.0,
                    srw_spd: 10.0,
                    srw: 18.0,
                },
                ShearRow {
                    label: "SFC-6km".into(),
                    ehi: 3.0,
                    srh: 350.0,
                    shear: 55.0,
                    mn_wind: 28.0,
                    srw_dir: 131.0,
                    srw_spd: 17.0,
                    srw: 24.0,
                },
                ShearRow {
                    label: "SFC-8km".into(),
                    ehi: 3.2,
                    srh: 380.0,
                    shear: 60.0,
                    mn_wind: 32.0,
                    srw_dir: 145.0,
                    srw_spd: 20.0,
                    srw: 28.0,
                },
            ],
            pw: 1.45,
            mean_w: 12.3,
            sfc_rh: 60.0,
            low_rh: 72.0,
            mid_rh: 55.0,
            dgz_rh: 50.0,
            freezing_level_m: 3500.0,
            wb_zero_m: 2900.0,
            mu_mpl_m: 12_500.0,
            thetae_diff_3km: 14.0,
            lcl_temp_c: 13.0,
            dcape: 850.0,
            dwn_t: 62.5,
            k_index: 32.0,
            t_totals: 52.0,
            tei: 28.0,
            tehi: f64::NAN,
            tts: f64::NAN,
            vtp_mod: f64::NAN,
            conv_t: 84.0,
            max_t: 88.0,
            mmp: 0.72,
            sig_svr: 45000.0,
            esp: 2.5,
            wndg: 1.3,
            dcp: 1.5,
            lhp: f64::NAN,
            cape_3km: 80.0,
            lapse_rates: vec![
                LapseRateRow {
                    label: "Sfc-3km".into(),
                    value: 7.8,
                },
                LapseRateRow {
                    label: "3-6km".into(),
                    value: 7.2,
                },
                LapseRateRow {
                    label: "850-500mb".into(),
                    value: 7.5,
                },
                LapseRateRow {
                    label: "700-500mb".into(),
                    value: 7.0,
                },
            ],
            bunkers_right: StormMotion {
                label: "Bunkers Right".into(),
                direction: 240.0,
                speed: 28.0,
            },
            bunkers_left: StormMotion {
                label: "Bunkers Left".into(),
                direction: 290.0,
                speed: 35.0,
            },
            corfidi_down: StormMotion {
                label: "Corfidi Downshear".into(),
                direction: 260.0,
                speed: 42.0,
            },
            corfidi_up: StormMotion {
                label: "Corfidi Upshear".into(),
                direction: 200.0,
                speed: 18.0,
            },
            stp_cin: 3.2,
            stp_fix: 2.8,
            ship: 1.5,
            scp: 12.0,
            brn_shear: 45.0,
            wind_1km_dir: 190.0,
            wind_1km_spd: 18.0,
            wind_6km_dir: 260.0,
            wind_6km_spd: 55.0,
        }
    }

    #[test]
    fn render_produces_correct_dimensions() {
        let data = sample_data();
        let buf = render(&data);
        assert_eq!(buf.width, TABLE_WIDTH);
        assert_eq!(buf.height, TABLE_HEIGHT);
        assert_eq!(buf.data.len(), TABLE_WIDTH * TABLE_HEIGHT * 4);
    }

    #[test]
    fn render_not_all_black() {
        let data = sample_data();
        let buf = render(&data);
        // At least some pixels should be non-black (text was drawn)
        let non_black = buf
            .data
            .chunks_exact(4)
            .any(|px| px[0] > 0 || px[1] > 0 || px[2] > 0);
        assert!(non_black, "Rendered table should contain non-black pixels");
    }

    #[test]
    fn blit_into_works() {
        let data = sample_data();
        let dst_w = 2400;
        let dst_h = 1800;
        let mut dst = vec![0u8; dst_w * dst_h * 4];
        blit_into(&mut dst, dst_w, 0, 1164, &data);
        // Check that some pixels in the blit region are non-zero
        let row = 1170;
        let col = 20;
        let off = (row * dst_w + col) * 4;
        let has_content = dst[off..off + 4].iter().any(|&b| b > 0);
        assert!(has_content, "Blitted region should have content");
    }

    #[test]
    fn fmt_missing_values() {
        assert_eq!(fmt_int(f64::NAN), "M");
        assert_eq!(fmt_1f(f64::NAN), "M");
        assert_eq!(fmt_2f(f64::NAN), "M");
        assert_eq!(fmt_int(crate::constants::MISSING), "M");
    }

    #[test]
    fn fmt_normal_values() {
        assert_eq!(fmt_int(1234.7), "1235");
        assert_eq!(fmt_1f(7.83), "7.8");
        assert_eq!(fmt_2f(1.459), "1.46");
    }

    #[test]
    fn pixel_buf_hline() {
        let mut buf = PixelBuf::new(100, 50, BLACK);
        buf.hline(10, 90, 25, WHITE);
        // Check middle of line
        let off = (25 * 100 + 50) * 4;
        assert_eq!(&buf.data[off..off + 4], &WHITE);
    }

    #[test]
    fn draw_str_advances() {
        let mut buf = PixelBuf::new(400, 40, BLACK);
        let end_x = buf.draw_str("CAPE", 0, 0, WHITE);
        assert_eq!(end_x, 4 * SCALED_CW);
    }

    #[test]
    fn render_sized_custom() {
        let data = sample_data();
        let buf = render_sized(&data, 2400, 636);
        assert_eq!(buf.width, 2400);
        assert_eq!(buf.height, 636);
        // Should have content
        let non_black = buf
            .data
            .chunks_exact(4)
            .any(|px| px[0] > 0 || px[1] > 0 || px[2] > 0);
        assert!(non_black);
    }

    #[test]
    fn color_coding_works() {
        // CAPE color thresholds
        assert_eq!(cape_color(1000.0), WHITE);
        assert_eq!(cape_color(2500.0), YELLOW);
        assert_eq!(cape_color(3500.0), ORANGE);
        assert_eq!(cape_color(5000.0), RED);

        // STP color thresholds
        assert_eq!(stp_color(0.5), WHITE);
        assert_eq!(stp_color(1.5), YELLOW);
        assert_eq!(stp_color(3.0), ORANGE);
        assert_eq!(stp_color(5.0), RED);
    }
}
