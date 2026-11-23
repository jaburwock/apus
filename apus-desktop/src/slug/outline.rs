//! Glyph outlines as closed quadratic Bézier contours in em space.
//!
//! `ttf-parser` yields move/line/quad/cubic drawing commands in font units (y-up, origin on the
//! baseline). The Slug fragment shader consumes quadratic Béziers only, so straight lines become
//! degenerate quadratics `(p1, p3, p3)` and cubics are subdivided into quadratics within
//! [`CUBIC_TOLERANCE_EM`].
//!
//! Every emitted control point is quantized to IEEE binary16 *before* leaving this module so band
//! assignment downstream decides on the exact coordinates the GPU reads from `Rgba16Float`.

use ttf_parser::{Face, GlyphId, OutlineBuilder};

use super::half::to_half_precision;

/// Maximum permitted deviation when replacing a cubic Bézier with quadratics, in em.
pub const CUBIC_TOLERANCE_EM: f32 = 1.0 / 4096.0;

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

/// A quadratic Bézier curve: `p1` and `p3` on the curve, `p2` the control point.
/// Straight lines repeat `p3` as `p2` (`(p1, p3, p3)`), which the shader's root solver handles
/// without a special case.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Curve {
    pub p1: Point,
    pub p2: Point,
    pub p3: Point,
}

/// A closed contour of consecutive curves.
pub type Contour = Vec<Curve>;

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct EmBox {
    pub min: Point,
    pub max: Point,
}

/// Bounding box over every control point (which bounds the curves by the convex-hull property).
pub fn bounds(curves: &[Curve]) -> EmBox {
    let Some(first) = curves.first() else {
        return EmBox::default();
    };
    let mut b = EmBox {
        min: first.p1,
        max: first.p1,
    };
    for c in curves {
        for p in [c.p1, c.p2, c.p3] {
            b.min.x = b.min.x.min(p.x);
            b.min.y = b.min.y.min(p.y);
            b.max.x = b.max.x.max(p.x);
            b.max.y = b.max.y.max(p.y);
        }
    }
    b
}

fn lerp(a: Point, b: Point, t: f32) -> Point {
    Point {
        x: a.x + (b.x - a.x) * t,
        y: a.y + (b.y - a.y) * t,
    }
}

struct Cubic {
    p0: Point,
    c1: Point,
    c2: Point,
    p3: Point,
}

impl Cubic {
    /// De Casteljau blossom so each piece's error stays independent of repeated subdivision.
    fn blossom(&self, a: f32, b: f32, c: f32) -> Point {
        let q0 = lerp(self.p0, self.c1, a);
        let q1 = lerp(self.c1, self.c2, a);
        let q2 = lerp(self.c2, self.p3, a);
        lerp(lerp(q0, q1, b), lerp(q1, q2, b), c)
    }

    /// Analytic piece count from the cubic's third difference `D = p3 - 3 c2 + 3 c1 - p0`:
    /// peak error of the midpoint quadratic is `sqrt(3) / 36 * |D|` and scales as `1 / n^3`.
    fn pieces_needed(&self) -> usize {
        let dx = self.p3.x - 3.0 * self.c2.x + 3.0 * self.c1.x - self.p0.x;
        let dy = self.p3.y - 3.0 * self.c2.y + 3.0 * self.c1.y - self.p0.y;
        let error_whole = (3.0f32.sqrt() / 36.0) * dx.hypot(dy);
        let n = (error_whole / CUBIC_TOLERANCE_EM).cbrt().ceil();
        (n as usize).max(1)
    }
}

/// Subdivides a cubic Bézier (in em) into consecutive quadratics within [`CUBIC_TOLERANCE_EM`].
/// Not yet quantized to half precision.
pub fn cubic_to_quadratics(p0: Point, c1: Point, c2: Point, p3: Point) -> Vec<Curve> {
    let cubic = Cubic { p0, c1, c2, p3 };
    let n = cubic.pieces_needed();
    let mut pieces = Vec::with_capacity(n);
    let mut start = p0;
    for i in 0..n {
        let a = i as f32 / n as f32;
        let b = (i + 1) as f32 / n as f32;
        let end = if i + 1 == n { p3 } else { cubic.blossom(b, b, b) };
        let inner1 = cubic.blossom(a, a, b);
        let inner2 = cubic.blossom(a, b, b);
        let control = Point {
            x: (3.0 * (inner1.x + inner2.x) - start.x - end.x) / 4.0,
            y: (3.0 * (inner1.y + inner2.y) - start.y - end.y) / 4.0,
        };
        pieces.push(Curve {
            p1: start,
            p2: control,
            p3: end,
        });
        start = end;
    }
    pieces
}

fn rounded(p: Point) -> Point {
    Point {
        x: to_half_precision(p.x),
        y: to_half_precision(p.y),
    }
}

fn collapsed(c: Curve) -> bool {
    c.p1 == c.p2 && c.p2 == c.p3
}

struct ContourBuilder {
    em_per_unit: f32,
    contours: Vec<Contour>,
    start: Point,
    at: Point,
    in_contour: bool,
}

impl ContourBuilder {
    fn new(em_per_unit: f32) -> Self {
        Self {
            em_per_unit,
            contours: Vec::new(),
            start: Point::default(),
            at: Point::default(),
            in_contour: false,
        }
    }

    fn em(&self, x: f32, y: f32) -> Point {
        Point {
            x: x * self.em_per_unit,
            y: y * self.em_per_unit,
        }
    }

    fn push_curve(&mut self, curve: Curve) {
        let c = Curve {
            p1: rounded(curve.p1),
            p2: rounded(curve.p2),
            p3: rounded(curve.p3),
        };
        if !collapsed(c)
            && let Some(contour) = self.contours.last_mut()
        {
            contour.push(c);
        }
    }

    fn finish(mut self) -> Vec<Contour> {
        self.contours.retain(|c| !c.is_empty());
        self.contours
    }
}

impl OutlineBuilder for ContourBuilder {
    fn move_to(&mut self, x: f32, y: f32) {
        if self.contours.last().is_none_or(|c| !c.is_empty()) {
            self.contours.push(Vec::new());
        }
        let p = self.em(x, y);
        self.start = p;
        self.at = p;
        self.in_contour = true;
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let to = self.em(x, y);
        self.push_curve(Curve {
            p1: self.at,
            p2: to,
            p3: to,
        });
        self.at = to;
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let ctrl = self.em(x1, y1);
        let to = self.em(x, y);
        self.push_curve(Curve {
            p1: self.at,
            p2: ctrl,
            p3: to,
        });
        self.at = to;
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let c1 = self.em(x1, y1);
        let c2 = self.em(x2, y2);
        let to = self.em(x, y);
        for piece in cubic_to_quadratics(self.at, c1, c2, to) {
            self.push_curve(piece);
        }
        self.at = to;
    }

    fn close(&mut self) {
        if self.in_contour {
            let start = self.start;
            self.push_curve(Curve {
                p1: self.at,
                p2: start,
                p3: start,
            });
            self.at = start;
            self.in_contour = false;
        }
    }
}

/// Extracts a glyph's closed quadratic contours in half-quantized em space.
pub fn glyph_contours(face: &Face<'_>, glyph_id: GlyphId, em_per_unit: f32) -> Vec<Contour> {
    let mut builder = ContourBuilder::new(em_per_unit);
    let _ = face.outline_glyph(glyph_id, &mut builder);
    builder.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval_cubic(p0: Point, c1: Point, c2: Point, p3: Point, t: f32) -> Point {
        let u = 1.0 - t;
        Point {
            x: u * u * u * p0.x + 3.0 * u * u * t * c1.x + 3.0 * u * t * t * c2.x + t * t * t * p3.x,
            y: u * u * u * p0.y + 3.0 * u * u * t * c1.y + 3.0 * u * t * t * c2.y + t * t * t * p3.y,
        }
    }

    fn eval_quad(c: Curve, t: f32) -> Point {
        let u = 1.0 - t;
        Point {
            x: u * u * c.p1.x + 2.0 * u * t * c.p2.x + t * t * c.p3.x,
            y: u * u * c.p1.y + 2.0 * u * t * c.p2.y + t * t * c.p3.y,
        }
    }

    #[test]
    fn cubic_subdivision_stays_within_tolerance_and_preserves_joins() {
        let p0 = Point { x: 0.0, y: 0.0 };
        let c1 = Point { x: 0.1, y: 1.2 };
        let c2 = Point { x: 0.9, y: -0.5 };
        let p3 = Point { x: 1.0, y: 0.8 };

        let quads = cubic_to_quadratics(p0, c1, c2, p3);
        assert!(quads.len() > 1, "high-inflection cubic must subdivide");
        assert_eq!(quads.first().unwrap().p1, p0);
        assert_eq!(quads.last().unwrap().p3, p3);

        for pair in quads.windows(2) {
            assert_eq!(pair[0].p3, pair[1].p1, "adjacent quadratics must share exact joins");
        }

        let n = quads.len();
        for (i, quad) in quads.iter().enumerate() {
            for s in 0..=16 {
                let local_t = s as f32 / 16.0;
                let global_t = (i as f32 + local_t) / n as f32;
                let a = eval_cubic(p0, c1, c2, p3, global_t);
                let b = eval_quad(*quad, local_t);
                let dist = (a.x - b.x).hypot(a.y - b.y);
                assert!(
                    dist <= CUBIC_TOLERANCE_EM + 1e-6,
                    "piece {i} t={local_t}: deviation {dist} exceeded tolerance {CUBIC_TOLERANCE_EM}"
                );
            }
        }
    }
}
