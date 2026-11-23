//! Bucketing a glyph's curves into overlapping horizontal and vertical bands.
//!
//! Three invariants are required by `glyph.wgsl` for correctness (not mere tuning):
//! 1. Horizontal bands are sorted by descending max-x and vertical bands by descending max-y so the
//!    shader can stop walking a band at the first curve wholly behind the pixel.
//! 2. Adjacent bands overlap by [`BAND_OVERLAP_EM`] so float rounding at a band boundary never
//!    drops a curve and leaves a hairline gap.
//! 3. Curves parallel to a ray (horizontal lines in horizontal bands, vertical lines in vertical
//!    bands) are omitted because a ray cannot cross them.

use super::outline::{Curve, EmBox, bounds};

/// Band overlap margin in em (`1/1024` em).
pub const BAND_OVERLAP_EM: f32 = 1.0 / 1024.0;

/// Maximum bands per axis (highest band index fits in 8 bits).
pub const MAX_BANDS: usize = 256;

/// Em-to-band-index affine transform: `index = em * scale + offset`, truncated and clamped.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BandTransform {
    pub scale_x: f32,
    pub scale_y: f32,
    pub offset_x: f32,
    pub offset_y: f32,
}

/// Band lists holding indices into the glyph's flat curve slice.
/// `horizontal[0]` is the bottom band; `vertical[0]` is the leftmost.
#[derive(Debug, Clone)]
pub struct GlyphBands {
    pub tiled: EmBox,
    pub transform: BandTransform,
    pub horizontal: Vec<Vec<u32>>,
    pub vertical: Vec<Vec<u32>>,
}

struct Span1d {
    min: f32,
    max: f32,
}

fn along_y(c: &Curve) -> Span1d {
    Span1d {
        min: c.p1.y.min(c.p2.y).min(c.p3.y),
        max: c.p1.y.max(c.p2.y).max(c.p3.y),
    }
}

fn along_x(c: &Curve) -> Span1d {
    Span1d {
        min: c.p1.x.min(c.p2.x).min(c.p3.x),
        max: c.p1.x.max(c.p2.x).max(c.p3.x),
    }
}

fn bands_per_axis(curve_count: usize) -> usize {
    let root = (curve_count as f64).sqrt().ceil() as usize;
    root.clamp(1, MAX_BANDS)
}

fn strips(
    curves: &[Curve],
    lo: f32,
    hi: f32,
    count: usize,
    across: fn(&Curve) -> Span1d,
    along: fn(&Curve) -> Span1d,
) -> Vec<Vec<u32>> {
    let mut out = vec![Vec::new(); count];
    let width = (hi - lo) / count as f32;
    for (i, band) in out.iter_mut().enumerate() {
        let band_lo = lo + width * i as f32 - BAND_OVERLAP_EM;
        let band_hi = lo + width * (i + 1) as f32 + BAND_OVERLAP_EM;
        for (k, curve) in curves.iter().enumerate() {
            let extent = across(curve);
            let parallel_line = extent.min == extent.max;
            if !parallel_line && extent.max >= band_lo && extent.min <= band_hi {
                band.push(k as u32);
            }
        }
        band.sort_by(|&a, &b| {
            let ma = along(&curves[a as usize]).max;
            let mb = along(&curves[b as usize]).max;
            mb.total_cmp(&ma)
        });
    }
    out
}

fn scale_for(extent: f32, count: usize) -> f32 {
    if extent > 0.0 { count as f32 / extent } else { 0.0 }
}

/// Tiles the bounding box of `curves` into `ceil(sqrt(n))` bands along each axis.
/// An empty slice produces one empty band per axis so every glyph shares the same layout.
pub fn make_bands(curves: &[Curve]) -> GlyphBands {
    let tiled = bounds(curves);
    let count = bands_per_axis(curves.len());
    let horizontal = strips(curves, tiled.min.y, tiled.max.y, count, along_y, along_x);
    let vertical = strips(curves, tiled.min.x, tiled.max.x, count, along_x, along_y);

    let scale_x = scale_for(tiled.max.x - tiled.min.x, count);
    let scale_y = scale_for(tiled.max.y - tiled.min.y, count);
    let transform = BandTransform {
        scale_x,
        scale_y,
        offset_x: -tiled.min.x * scale_x,
        offset_y: -tiled.min.y * scale_y,
    };

    GlyphBands {
        tiled,
        transform,
        horizontal,
        vertical,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slug::outline::Point;

    #[test]
    fn bands_drop_parallel_lines_overlap_boundaries_and_sort_descending() {
        // Closed unit square: bottom/top are horizontal, left/right are vertical.
        let p00 = Point { x: 0.0, y: 0.0 };
        let p10 = Point { x: 1.0, y: 0.0 };
        let p11 = Point { x: 1.0, y: 1.0 };
        let p01 = Point { x: 0.0, y: 1.0 };
        let curves = vec![
            Curve {
                p1: p00,
                p2: p10,
                p3: p10,
            }, // 0: horizontal (y=0)
            Curve {
                p1: p10,
                p2: p11,
                p3: p11,
            }, // 1: vertical (x=1)
            Curve {
                p1: p11,
                p2: p01,
                p3: p01,
            }, // 2: horizontal (y=1)
            Curve {
                p1: p01,
                p2: p00,
                p3: p00,
            }, // 3: vertical (x=0)
        ];

        let bands = make_bands(&curves);
        assert_eq!(bands.horizontal.len(), 2);
        assert_eq!(bands.vertical.len(), 2);

        // Horizontal bands hold only the two vertical edges, sorted by descending max-x: [1, 3].
        for hband in &bands.horizontal {
            assert_eq!(hband, &[1, 3]);
        }
        // Vertical bands hold only the two horizontal edges, sorted by descending max-y: [2, 0].
        for vband in &bands.vertical {
            assert_eq!(vband, &[2, 0]);
        }
    }

    #[test]
    fn curve_on_exact_band_boundary_appears_in_both_adjacent_bands() {
        // 4 curves => 2 bands per axis splitting at y = 0.5.
        let curves = vec![
            Curve {
                p1: Point { x: 0.0, y: 0.0 },
                p2: Point { x: 0.0, y: 0.5 },
                p3: Point { x: 0.0, y: 0.5 },
            },
            Curve {
                p1: Point { x: 0.0, y: 0.5 },
                p2: Point { x: 0.0, y: 1.0 },
                p3: Point { x: 0.0, y: 1.0 },
            },
            Curve {
                p1: Point { x: 1.0, y: 1.0 },
                p2: Point { x: 1.0, y: 0.5 },
                p3: Point { x: 1.0, y: 0.5 },
            },
            Curve {
                p1: Point { x: 1.0, y: 0.5 },
                p2: Point { x: 1.0, y: 0.0 },
                p3: Point { x: 1.0, y: 0.0 },
            },
        ];

        let bands = make_bands(&curves);
        // Because of BAND_OVERLAP_EM, curves touching y = 0.5 appear in both horizontal bands.
        assert!(bands.horizontal[0].contains(&1));
        assert!(bands.horizontal[1].contains(&0));
    }
}
