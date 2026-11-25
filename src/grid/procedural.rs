//! Procedural pixel-snapped glyph decoders and integer-edge geometry.
//!
//! Quadrants, half/full blocks, Unicode 16 octants, straight box-drawing lines, Braille patterns,
//! and text rules are emitted as exact integer-pixel rectangles so adjacent terminal cells meet
//! with zero 1 px seams, and half-cell/octant boundaries land on the same pixel edges in every
//! row and column across both CPU and GPU renderers.

use super::font::CellMetrics;

/// Axis-aligned half-open rectangle `[x0, x1) × [y0, y1)` in pixel coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelRect {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

/// Decodes any Unicode 16 octant character (including the 16 quadrant/half/full blocks and
/// quarter/three-quarter blocks that share the octant table) into its 2×4 row-major bitmask:
///
/// ```text
///   bit 0 | bit 1   (row 0, top)
///   bit 2 | bit 3   (row 1)
///   bit 4 | bit 5   (row 2)
///   bit 6 | bit 7   (row 3, bottom)
/// ```
///
/// Returns `None` for space (`0x00`) and non-octant characters.
pub fn octant_mask(ch: char) -> Option<u8> {
    let mask = match ch {
        // Scattered existing Unicode codepoints reused by the Unicode 16 octant table.
        '\u{1CEA8}' => 0x01, // 𜺨
        '\u{1CEAB}' => 0x02, // 𜺫
        '\u{1FB82}' => 0x03, // 🮂 upper one quarter block
        '\u{2598}' => 0x05,  // ▘ quadrant upper left
        '\u{259D}' => 0x0A,  // ▝ quadrant upper right
        '\u{2580}' => 0x0F,  // ▀ upper half block
        '\u{1FBE6}' => 0x14, // 🯦
        '\u{1FBE7}' => 0x28, // 🯧
        '\u{1FB85}' => 0x3F, // 🮅 upper three quarters block
        '\u{1CEA3}' => 0x40, // 𜺣
        '\u{2596}' => 0x50,  // ▖ quadrant lower left
        '\u{258C}' => 0x55,  // ▌ left half block
        '\u{259E}' => 0x5A,  // ▞ quadrant upper right and lower left
        '\u{259B}' => 0x5F,  // ▛ quadrant upper left and upper right and lower left
        '\u{1CEA0}' => 0x80, // 𜺠
        '\u{2597}' => 0xA0,  // ▗ quadrant lower right
        '\u{259A}' => 0xA5,  // ▚ quadrant upper left and lower right
        '\u{2590}' => 0xAA,  // ▐ right half block
        '\u{259C}' => 0xAF,  // ▜ quadrant upper left and upper right and lower right
        '\u{2582}' => 0xC0,  // ▂ lower one quarter block
        '\u{2584}' => 0xF0,  // ▄ lower half block
        '\u{2599}' => 0xF5,  // ▙ quadrant upper left and lower left and lower right
        '\u{259F}' => 0xFA,  // ▟ quadrant upper right and lower left and lower right
        '\u{2586}' => 0xFC,  // ▆ lower three quarters block
        '\u{2588}' => 0xFF,  // █ full block
        // The 230 contiguous Unicode 16 octant block codepoints (U+1CD00..=U+1CDE5) fill the
        // remaining non-zero masks in ascending order, skipping the 25 masks above.
        '\u{1CD00}'..='\u{1CDE5}' => {
            const SKIPPED: [u8; 25] = [
                0x01, 0x02, 0x03, 0x05, 0x0A, 0x0F, 0x14, 0x28, 0x3F, 0x40, 0x50, 0x55, 0x5A, 0x5F, 0x80, 0xA0, 0xA5,
                0xAA, 0xAF, 0xC0, 0xF0, 0xF5, 0xFA, 0xFC, 0xFF,
            ];
            let mut m = (ch as u32 - 0x1CD00 + 1) as u8;
            for &s in &SKIPPED {
                if m >= s {
                    m += 1;
                }
            }
            m
        }
        _ => return None,
    };
    Some(mask)
}

/// Decodes a Braille pattern (`U+2800..=U+28FF`) into its 8-bit dot mask.
/// Returns `Some(0)` for blank Braille `U+2800` so it does not fall through to the font rasterizer.
pub fn braille_mask(ch: char) -> Option<u8> {
    let cp = ch as u32;
    if (0x2800..=0x28FF).contains(&cp) {
        Some((cp - 0x2800) as u8)
    } else {
        None
    }
}

/// Weight of one arm of a straight box-drawing glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineWeight {
    None,
    Light,
    Heavy,
}

/// Cardinal arms of a straight box-drawing character.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoxArms {
    pub left: LineWeight,
    pub right: LineWeight,
    pub up: LineWeight,
    pub down: LineWeight,
}

/// Decodes straight box-drawing characters (`─━│┃┌┐└┘├┤┬┴┼`) into cardinal arms.
pub fn box_drawing_arms(ch: char) -> Option<BoxArms> {
    use LineWeight::{Heavy, Light, None as N};
    let arms = match ch {
        '─' => BoxArms {
            left: Light,
            right: Light,
            up: N,
            down: N,
        },
        '━' => BoxArms {
            left: Heavy,
            right: Heavy,
            up: N,
            down: N,
        },
        '│' => BoxArms {
            left: N,
            right: N,
            up: Light,
            down: Light,
        },
        '┃' => BoxArms {
            left: N,
            right: N,
            up: Heavy,
            down: Heavy,
        },
        '┌' => BoxArms {
            left: N,
            right: Light,
            up: N,
            down: Light,
        },
        '┐' => BoxArms {
            left: Light,
            right: N,
            up: N,
            down: Light,
        },
        '└' => BoxArms {
            left: N,
            right: Light,
            up: Light,
            down: N,
        },
        '┘' => BoxArms {
            left: Light,
            right: N,
            up: Light,
            down: N,
        },
        '├' => BoxArms {
            left: N,
            right: Light,
            up: Light,
            down: Light,
        },
        '┤' => BoxArms {
            left: Light,
            right: N,
            up: Light,
            down: Light,
        },
        '┬' => BoxArms {
            left: Light,
            right: Light,
            up: N,
            down: Light,
        },
        '┴' => BoxArms {
            left: Light,
            right: Light,
            up: Light,
            down: N,
        },
        '┼' => BoxArms {
            left: Light,
            right: Light,
            up: Light,
            down: Light,
        },
        _ => return None,
    };
    Some(arms)
}

/// Emits pixel-snapped rectangles for an octant bitmask within cell `[x0, x0 + cw) × [y0, y0 + ch)`.
///
/// Sub-cell edges are derived solely from `(cw, ch)` and added to `(x0, y0)` so every row and
/// column shares identical half-cell and quarter-cell boundaries.
pub fn octant_rects(mask: u8, x0: i32, y0: i32, cw: i32, ch: i32, mut emit: impl FnMut(PixelRect)) {
    let cw_f = cw as f32;
    let ch_f = ch as f32;
    let xs = [x0, x0 + (cw_f * 0.5).round() as i32, x0 + cw];
    let ys = [
        y0,
        y0 + (ch_f * 0.25).round() as i32,
        y0 + (ch_f * 0.5).round() as i32,
        y0 + (ch_f * 0.75).round() as i32,
        y0 + ch,
    ];

    // Row masks (2 bits per row: bit 0 = left, bit 1 = right), coalesced vertically.
    let rows = [mask & 0x03, (mask >> 2) & 0x03, (mask >> 4) & 0x03, (mask >> 6) & 0x03];
    let mut r = 0usize;
    while r < 4 {
        let rm = rows[r];
        let mut end = r + 1;
        while end < 4 && rows[end] == rm {
            end += 1;
        }
        match rm {
            0x01 => emit(PixelRect {
                x0: xs[0],
                y0: ys[r],
                x1: xs[1],
                y1: ys[end],
            }),
            0x02 => emit(PixelRect {
                x0: xs[1],
                y0: ys[r],
                x1: xs[2],
                y1: ys[end],
            }),
            0x03 => emit(PixelRect {
                x0: xs[0],
                y0: ys[r],
                x1: xs[2],
                y1: ys[end],
            }),
            _ => {}
        }
        r = end;
    }
}

/// Emits pixel-snapped dot rectangles for a Braille bitmask within cell `[x0, x0 + cw) × [y0, y0 + ch)`.
pub fn braille_rects(mask: u8, x0: i32, y0: i32, cw: i32, ch: i32, mut emit: impl FnMut(PixelRect)) {
    if mask == 0 {
        return;
    }
    let cw_f = cw as f32;
    let ch_f = ch as f32;
    let xs = [x0, x0 + (cw_f * 0.5).round() as i32, x0 + cw];
    let ys = [
        y0,
        y0 + (ch_f * 0.25).round() as i32,
        y0 + (ch_f * 0.5).round() as i32,
        y0 + (ch_f * 0.75).round() as i32,
        y0 + ch,
    ];
    let dw = (cw_f * 0.32).round().max(1.0).min((cw_f * 0.5 - 1.0).max(1.0)) as i32;
    let dh = (ch_f * 0.16).round().max(1.0).min((ch_f * 0.25 - 1.0).max(1.0)) as i32;

    const DOT_BITS: [[u8; 4]; 2] = [[0x01, 0x02, 0x04, 0x40], [0x08, 0x10, 0x20, 0x80]];
    for (col, bits) in DOT_BITS.iter().enumerate() {
        let dx = ((xs[col] + xs[col + 1] - dw) as f32 * 0.5).floor() as i32;
        for (row, &bit) in bits.iter().enumerate() {
            if (mask & bit) != 0 {
                let dy = ((ys[row] + ys[row + 1] - dh) as f32 * 0.5).floor() as i32;
                emit(PixelRect {
                    x0: dx,
                    y0: dy,
                    x1: dx + dw,
                    y1: dy + dh,
                });
            }
        }
    }
}

/// Emits pixel-snapped arm rectangles for straight box-drawing glyphs within cell `[x0, x0 + cw) × [y0, y0 + ch)`.
pub fn box_drawing_rects(arms: BoxArms, x0: i32, y0: i32, cw: i32, ch: i32, mut emit: impl FnMut(PixelRect)) {
    let light = (cw as f32 / 8.0).round().max(1.0) as i32;
    let heavy = (light * 2).max(2);
    let stroke = |w: LineWeight| match w {
        LineWeight::None => 0,
        LineWeight::Light => light,
        LineWeight::Heavy => heavy,
    };

    let h_thick = stroke(arms.left).max(stroke(arms.right)).max(light);
    let v_thick = stroke(arms.up).max(stroke(arms.down)).max(light);
    let vx0 = x0 + ((cw - v_thick) as f32 * 0.5).floor() as i32;
    let vx1 = vx0 + v_thick;
    let hy0 = y0 + ((ch - h_thick) as f32 * 0.5).floor() as i32;
    let hy1 = hy0 + h_thick;
    let x1 = x0 + cw;
    let y1 = y0 + ch;

    match (arms.left, arms.right) {
        (LineWeight::None, LineWeight::None) => {}
        (l, r) if l == r => emit(PixelRect {
            x0,
            y0: hy0,
            x1,
            y1: hy1,
        }),
        (LineWeight::None, _) => emit(PixelRect {
            x0: vx0,
            y0: hy0,
            x1,
            y1: hy1,
        }),
        (_, LineWeight::None) => emit(PixelRect {
            x0,
            y0: hy0,
            x1: vx1,
            y1: hy1,
        }),
        _ => {
            emit(PixelRect {
                x0,
                y0: hy0,
                x1: vx1,
                y1: hy1,
            });
            emit(PixelRect {
                x0: vx0,
                y0: hy0,
                x1,
                y1: hy1,
            });
        }
    }

    match (arms.up, arms.down) {
        (LineWeight::None, LineWeight::None) => {}
        (u, d) if u == d => emit(PixelRect {
            x0: vx0,
            y0,
            x1: vx1,
            y1,
        }),
        (LineWeight::None, _) => emit(PixelRect {
            x0: vx0,
            y0: hy0,
            x1: vx1,
            y1,
        }),
        (_, LineWeight::None) => emit(PixelRect {
            x0: vx0,
            y0,
            x1: vx1,
            y1: hy1,
        }),
        _ => {
            emit(PixelRect {
                x0: vx0,
                y0,
                x1: vx1,
                y1: hy1,
            });
            emit(PixelRect {
                x0: vx0,
                y0: hy0,
                x1: vx1,
                y1,
            });
        }
    }
}

/// Thickness in pixels for underline and strikethrough rules.
pub fn rule_thickness(cw: i32) -> i32 {
    (cw as f32 / 9.0).round().max(1.0) as i32
}

/// Underline rectangle for cell `(x0, y0)`.
pub fn underline_rect(x0: i32, y0: i32, metrics: CellMetrics) -> PixelRect {
    let cw = metrics.width_px.round().max(1.0) as i32;
    let ch = metrics.height_px.round().max(1.0) as i32;
    let thick = rule_thickness(cw);
    let baseline = metrics.baseline_px.round() as i32;
    let uy = (y0 + baseline + thick).min(y0 + ch - thick).max(y0);
    PixelRect {
        x0,
        y0: uy,
        x1: x0 + cw,
        y1: uy + thick,
    }
}

/// Strikethrough rectangle for cell `(x0, y0)`.
pub fn strikethrough_rect(x0: i32, y0: i32, metrics: CellMetrics) -> PixelRect {
    let cw = metrics.width_px.round().max(1.0) as i32;
    let thick = rule_thickness(cw);
    let sy = (y0 as f32 + metrics.baseline_px - metrics.size_px * 0.32).round() as i32;
    PixelRect {
        x0,
        y0: sy,
        x1: x0 + cw,
        y1: sy + thick,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn octant_decoder_matches_all_255_non_space_ratatui_octants() {
        assert_eq!(octant_mask(ratatui::symbols::pixel::OCTANTS[0]), None);
        for (mask, &ch) in ratatui::symbols::pixel::OCTANTS.iter().enumerate().skip(1) {
            assert_eq!(
                octant_mask(ch),
                Some(mask as u8),
                "octant U+{:04X} ({ch}) at mask 0x{mask:02x}",
                ch as u32
            );
        }
    }

    #[test]
    fn braille_decoder_covers_entire_block() {
        assert_eq!(braille_mask('\u{2800}'), Some(0x00));
        assert_eq!(braille_mask('\u{28FF}'), Some(0xFF));
        assert_eq!(braille_mask('\u{2836}'), Some(0x36));
        assert_eq!(braille_mask('A'), None);
    }
}
