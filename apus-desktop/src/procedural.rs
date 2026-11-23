//! Procedural pixel-snapped glyph decoders for the flat rect pipeline.
//!
//! Quadrants, half/full blocks, Unicode 16 octants, straight box-drawing lines, and Braille
//! patterns are emitted as exact integer-pixel rectangles so adjacent terminal cells meet with
//! zero 1 px seams.

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
/// Returns `Some(0)` for blank Braille `U+2800` so it does not fall through to Slug.
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
