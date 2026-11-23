//! Packing Slug curve and band data without straddling texture rows.
//!
//! The shader wraps only one address across rows: where a band's curve list starts relative to
//! the glyph's header location. Nothing else wraps — not a curve's second texel, not a glyph's
//! band header block, and not entries along a band's curve list. Every such sequence is padded to
//! the start of the next row before appending if it would cross the row boundary.

use super::bands::GlyphBands;
use super::font_data::{BandTexel, CurveTexel, TEXTURE_WIDTH, TexelLocation};
use super::half::to_half;
use super::outline::{Contour, Point};

fn narrow(value: usize, what: &str) -> u16 {
    u16::try_from(value).unwrap_or_else(|_| panic!("{what}: {value} exceeds u16::MAX"))
}

fn column(index: usize) -> usize {
    index % TEXTURE_WIDTH
}

fn location_of(index: usize) -> TexelLocation {
    TexelLocation {
        x: column(index) as u16,
        y: narrow(index / TEXTURE_WIDTH, "glyph data exceeds 65536 texture rows"),
    }
}

fn pad_to_next_row<T: Default + Clone>(texels: &mut Vec<T>) {
    let col = column(texels.len());
    if col != 0 {
        texels.resize(texels.len() + TEXTURE_WIDTH - col, T::default());
    }
}

fn curve_texel_pair(a: Point, b: Point) -> CurveTexel {
    [to_half(a.x), to_half(a.y), to_half(b.x), to_half(b.y)]
}

fn curve_texel_end(a: Point) -> CurveTexel {
    [to_half(a.x), to_half(a.y), 0, 0]
}

/// Appends each contour's curves into `texels` and returns the texel location of every curve in
/// contour order. No curve starts in the last column of a row; if a contour reaches it, the
/// previous curve's endpoint finishes that row and the next curve starts at column 0 of the next.
pub fn pack_curves(texels: &mut Vec<CurveTexel>, contours: &[Contour]) -> Vec<TexelLocation> {
    const LAST_COLUMN: usize = TEXTURE_WIDTH - 1;

    let mut locations = Vec::new();
    for curves in contours {
        for (k, curve) in curves.iter().enumerate() {
            if column(texels.len()) == LAST_COLUMN {
                let tail = if k == 0 {
                    CurveTexel::default()
                } else {
                    curve_texel_end(curves[k - 1].p3)
                };
                texels.push(tail);
            }
            locations.push(location_of(texels.len()));
            texels.push(curve_texel_pair(curve.p1, curve.p2));
        }
        if let Some(last) = curves.last() {
            texels.push(curve_texel_end(last.p3));
        }
    }
    locations
}

/// Appends one glyph's band headers and curve lists into `texels`, returning the header block's
/// location.
pub fn pack_bands(texels: &mut Vec<BandTexel>, bands: &GlyphBands, curve_locations: &[TexelLocation]) -> TexelLocation {
    let header_count = bands.horizontal.len() + bands.vertical.len();
    if column(texels.len()) + header_count > TEXTURE_WIDTH {
        pad_to_next_row(texels);
    }

    let glyph_start = texels.len();
    texels.resize(glyph_start + header_count, BandTexel::default());

    let mut header = glyph_start;
    for axis in [&bands.horizontal, &bands.vertical] {
        for list in axis {
            assert!(
                list.len() <= TEXTURE_WIDTH,
                "a glyph's band lists more curves ({}) than a texture row holds ({TEXTURE_WIDTH})",
                list.len()
            );
            if !list.is_empty() && column(texels.len()) + list.len() > TEXTURE_WIDTH {
                pad_to_next_row(texels);
            }

            let offset = if list.is_empty() { 0 } else { texels.len() - glyph_start };
            texels[header] = [
                list.len() as u16,
                narrow(offset, "a glyph's band lists lie too far from its headers"),
            ];
            header += 1;
            for &k in list {
                let loc = curve_locations[k as usize];
                texels.push([loc.x, loc.y]);
            }
        }
    }

    location_of(glyph_start)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slug::bands::make_bands;
    use crate::slug::outline::Curve;

    #[test]
    fn pack_curves_never_starts_a_curve_in_the_last_column_of_a_row() {
        let mut texels = vec![CurveTexel::default(); TEXTURE_WIDTH - 2];
        let contour = vec![
            Curve {
                p1: Point { x: 0.0, y: 0.0 },
                p2: Point { x: 0.5, y: 0.0 },
                p3: Point { x: 1.0, y: 0.0 },
            },
            Curve {
                p1: Point { x: 1.0, y: 0.0 },
                p2: Point { x: 1.0, y: 0.5 },
                p3: Point { x: 1.0, y: 1.0 },
            },
        ];

        let locs = pack_curves(&mut texels, &[contour]);
        assert_eq!(locs.len(), 2);
        // Curve 0 occupies the last two texels of row 0. Curve 1 starts at row 1 col 0.
        assert_eq!(
            locs[0],
            TexelLocation {
                x: (TEXTURE_WIDTH - 2) as u16,
                y: 0
            }
        );
        assert_eq!(locs[1], TexelLocation { x: 0, y: 1 });
        // The shader reads curve 0's endpoint at curve_loc.x + 1.
        assert_eq!(texels[TEXTURE_WIDTH - 1], curve_texel_end(Point { x: 1.0, y: 0.0 }));
        // Curve 1's p1,p2 sit at (0, 1) and its p3 at (1, 1).
        assert_eq!(
            texels[TEXTURE_WIDTH],
            curve_texel_pair(Point { x: 1.0, y: 0.0 }, Point { x: 1.0, y: 0.5 })
        );
        assert_eq!(texels[TEXTURE_WIDTH + 1], curve_texel_end(Point { x: 1.0, y: 1.0 }));
    }

    #[test]
    fn pack_bands_pads_headers_and_lists_that_would_cross_row_boundary() {
        let mut band_texels = vec![BandTexel::default(); TEXTURE_WIDTH - 1];
        let curves = vec![
            Curve {
                p1: Point { x: 0.0, y: 0.0 },
                p2: Point { x: 1.0, y: 0.0 },
                p3: Point { x: 1.0, y: 0.0 },
            },
            Curve {
                p1: Point { x: 1.0, y: 0.0 },
                p2: Point { x: 1.0, y: 1.0 },
                p3: Point { x: 1.0, y: 1.0 },
            },
        ];
        let bands = make_bands(&curves);
        let curve_locs = vec![TexelLocation { x: 0, y: 0 }, TexelLocation { x: 1, y: 0 }];

        let header_loc = pack_bands(&mut band_texels, &bands, &curve_locs);
        // 4 headers cannot fit in 1 remaining column, so headers must move to row 1 col 0.
        assert_eq!(header_loc, TexelLocation { x: 0, y: 1 });
    }
}
