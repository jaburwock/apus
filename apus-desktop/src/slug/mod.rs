//! Slug outline text engine: CPU font packing and per-glyph quad builder.

mod bands;
mod font_data;
mod half;
mod outline;
mod pack;

use bytemuck::{Pod, Zeroable};

pub use font_data::{FontData, GlyphRecord, TEXTURE_WIDTH, build_embedded_fonts};
pub use outline::Point;

/// Vertex format matching `shaders/glyph.wgsl`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct GlyphVertex {
    pub position: [f32; 2],
    pub normal: [f32; 2],
    pub em: [f32; 2],
    /// Em per physical pixel, rows `(xx, xy)` and `(yx, yy)`.
    pub inverse_jacobian: [f32; 4],
    /// Em to band index: `(scale_x, scale_y, offset_x, offset_y)`.
    pub band_transform: [f32; 4],
    /// Premultiplied RGBA colour.
    pub colour: [f32; 4],
    /// `[column | row << 16, band_max_x | band_max_y << 16]`.
    pub band_lookup: [u32; 2],
}

/// Builds the 4 vertices (top-left, top-right, bottom-left, bottom-right) for one Slug glyph quad
/// placed with its baseline origin at `origin_px` and em size `size_px` in physical pixels.
pub fn make_glyph_quad(glyph: &GlyphRecord, origin_px: Point, size_px: f32, colour: [f32; 4]) -> [GlyphVertex; 4] {
    let b = glyph.quad;
    let lookup = [
        u32::from(glyph.location.x) | (u32::from(glyph.location.y) << 16),
        u32::from(glyph.band_max_x) | (u32::from(glyph.band_max_y) << 16),
    ];
    let inv_j = [1.0 / size_px, 0.0, 0.0, -1.0 / size_px];
    let bt = [
        glyph.bands.scale_x,
        glyph.bands.scale_y,
        glyph.bands.offset_x,
        glyph.bands.offset_y,
    ];

    let corner = |em_x: f32, em_y: f32, nx: f32, ny: f32| GlyphVertex {
        position: [origin_px.x + em_x * size_px, origin_px.y - em_y * size_px],
        normal: [nx, ny],
        em: [em_x, em_y],
        inverse_jacobian: inv_j,
        band_transform: bt,
        colour,
        band_lookup: lookup,
    };

    [
        corner(b.min.x, b.max.y, -1.0, -1.0),
        corner(b.max.x, b.max.y, 1.0, -1.0),
        corner(b.min.x, b.min.y, -1.0, 1.0),
        corner(b.max.x, b.min.y, 1.0, 1.0),
    ]
}
