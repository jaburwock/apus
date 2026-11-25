//! Fixed 256-entry ANSI colour lookup table.
//!
//! Everything below the thin Ratatui backend adapter deals solely in `u8` ANSI-256 indices.
//! Indices 16–231 use the standard xterm 6×6×6 cube, 232–255 use the 24-step greyscale ramp,
//! and 0–15 use a standard dark-terminal palette so user-selected overrides still resolve
//! branchlessly via `RGB_LUT[idx as usize]`.

/// Standard dark-terminal RGB values for ANSI indices 0–15.
const SYSTEM_16: [[u8; 3]; 16] = [
    [0x00, 0x00, 0x00], // 0  Black
    [0xcd, 0x00, 0x00], // 1  Red
    [0x00, 0xcd, 0x00], // 2  Green
    [0xcd, 0xcd, 0x00], // 3  Yellow
    [0x00, 0x00, 0xee], // 4  Blue
    [0xcd, 0x00, 0xcd], // 5  Magenta
    [0x00, 0xcd, 0xcd], // 6  Cyan
    [0xe5, 0xe5, 0xe5], // 7  White
    [0x7f, 0x7f, 0x7f], // 8  Bright Black
    [0xff, 0x00, 0x00], // 9  Bright Red
    [0x00, 0xff, 0x00], // 10 Bright Green
    [0xff, 0xff, 0x00], // 11 Bright Yellow
    [0x5c, 0x5c, 0xff], // 12 Bright Blue
    [0xff, 0x00, 0xff], // 13 Bright Magenta
    [0x00, 0xff, 0xff], // 14 Bright Cyan
    [0xff, 0xff, 0xff], // 15 Bright White
];

/// Standard xterm 6-level cube channel values.
const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

const fn build_rgb_lut() -> [[u8; 3]; 256] {
    let mut lut = [[0u8; 3]; 256];
    let mut i = 0usize;
    while i < 16 {
        lut[i] = SYSTEM_16[i];
        i += 1;
    }
    while i < 232 {
        let n = i - 16;
        let r = n / 36;
        let g = (n / 6) % 6;
        let b = n % 6;
        lut[i] = [CUBE_LEVELS[r], CUBE_LEVELS[g], CUBE_LEVELS[b]];
        i += 1;
    }
    while i < 256 {
        let v = (8 + 10 * (i - 232)) as u8;
        lut[i] = [v, v, v];
        i += 1;
    }
    lut
}

/// Complete 256-entry sRGB colour lookup table.
pub const RGB_LUT: [[u8; 3]; 256] = build_rgb_lut();

/// Fallback foreground index when a cell carries a non-indexed colour.
pub const FALLBACK_FG: u8 = 252;

/// Fallback background index when a cell carries a non-indexed colour.
pub const FALLBACK_BG: u8 = 234;

/// Foreground opacity factor for [`super::CellFlags::DIM`].
pub const DIM_ALPHA: f32 = 0.65;

fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Resolves an ANSI-256 index to premultiplied RGBA floats for the target render format.
///
/// When `target_is_srgb` is true, converts sRGB LUT channels to linear so
/// hardware sRGB encoding reproduces the exact ANSI byte values. When false (`Rgba8Unorm`), emits
/// normalized sRGB floats directly.
pub fn rgba(index: u8, alpha: f32, target_is_srgb: bool) -> [f32; 4] {
    let [r, g, b] = RGB_LUT[usize::from(index)];
    let (mut rf, mut gf, mut bf) = (f32::from(r) / 255.0, f32::from(g) / 255.0, f32::from(b) / 255.0);
    if target_is_srgb {
        rf = srgb_to_linear(rf);
        gf = srgb_to_linear(gf);
        bf = srgb_to_linear(bf);
    }
    [rf * alpha, gf * alpha, bf * alpha, alpha]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_256_cube_and_greyscale_endpoints_match_xterm_spec() {
        // Cube corners
        assert_eq!(RGB_LUT[16], [0, 0, 0]);
        assert_eq!(RGB_LUT[21], [0, 0, 255]);
        assert_eq!(RGB_LUT[46], [0, 255, 0]);
        assert_eq!(RGB_LUT[196], [255, 0, 0]);
        assert_eq!(RGB_LUT[231], [255, 255, 255]);
        // Greyscale endpoints
        assert_eq!(RGB_LUT[232], [8, 8, 8]);
        assert_eq!(RGB_LUT[255], [238, 238, 238]);
    }
}
