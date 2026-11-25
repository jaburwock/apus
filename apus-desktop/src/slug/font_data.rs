//! Upfront font loading and Slug texture packing for the embedded Fira Code faces.
//!
//! Both Regular and Bold faces are packed once at startup into one immutable curve/band texture
//! pair. Only mapped Unicode codepoints are packed (skipping Fira Code's ~444 unmapped ligature
//! glyphs), and UI fallbacks (such as `▸` -> `▶`) are resolved directly in the lookup table.

use std::collections::{BTreeMap, HashMap};

use apus::grid::{FIRA_CODE_BOLD, FIRA_CODE_REGULAR, FontMetrics, fallback_char};
use ttf_parser::{Face, GlyphId};

use super::bands::{BandTransform, make_bands};
use super::outline::{EmBox, glyph_contours};
use super::pack::{pack_bands, pack_curves};

/// Width of both Slug textures in texels, within the WebGL2 baseline limits.
pub const TEXTURE_WIDTH: usize = 2048;

/// `Rgba16Float` curve texel as IEEE binary16 bit patterns: `(p1.x, p1.y, p2.x, p2.y)` or
/// `(p3.x, p3.y, 0, 0)` at a contour's final curve / row wrap.
pub type CurveTexel = [u16; 4];

/// `Rg16Uint` band texel: either a header `(curve_count, list_offset)` or a list entry `(x, y)`.
pub type BandTexel = [u16; 2];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TexelLocation {
    pub x: u16,
    pub y: u16,
}

/// Per-glyph metadata required by the vertex builder.
#[derive(Debug, Clone, Copy, Default)]
pub struct GlyphRecord {
    /// Em-space bounding box of the glyph's outline before shader dilation.
    pub quad: EmBox,
    pub bands: BandTransform,
    /// Header block location in the band texture (horizontal headers first, then vertical).
    pub location: TexelLocation,
    pub band_max_x: u16,
    pub band_max_y: u16,
    /// True when the glyph has no outline curves (e.g. space) and needs no quad.
    pub empty: bool,
}

/// Packed GPU buffers and codepoint lookup tables for Regular and Bold faces.
pub struct FontData {
    pub metrics: FontMetrics,
    pub curve_texels: Vec<CurveTexel>,
    pub band_texels: Vec<BandTexel>,
    glyphs: Vec<GlyphRecord>,
    regular: HashMap<char, u32>,
    bold: HashMap<char, u32>,
    fallback_regular: u32,
    fallback_bold: u32,
}

impl FontData {
    /// Looks up the packed [`GlyphRecord`] for `ch` in Regular or Bold.
    pub fn glyph(&self, ch: char, bold: bool) -> &GlyphRecord {
        let mapped = fallback_char(ch);
        let table = if bold { &self.bold } else { &self.regular };
        let idx = table
            .get(&mapped)
            .or_else(|| self.regular.get(&mapped))
            .copied()
            .unwrap_or(if bold {
                self.fallback_bold
            } else {
                self.fallback_regular
            });
        &self.glyphs[idx as usize]
    }

    pub fn curve_texture_height(&self) -> u32 {
        (self.curve_texels.len() / TEXTURE_WIDTH).max(1) as u32
    }

    pub fn band_texture_height(&self) -> u32 {
        (self.band_texels.len() / TEXTURE_WIDTH).max(1) as u32
    }

    #[cfg(test)]
    pub fn glyph_count(&self) -> usize {
        self.glyphs.len()
    }
}

fn pad_to_whole_rows<T: Default + Clone>(texels: &mut Vec<T>) {
    if texels.is_empty() {
        texels.resize(TEXTURE_WIDTH, T::default());
        return;
    }
    let rows = texels.len().div_ceil(TEXTURE_WIDTH);
    texels.resize(rows * TEXTURE_WIDTH, T::default());
}

/// Collects every Unicode codepoint mapped by `face` and deduplicates by `GlyphId`.
fn collect_mapped_codepoints(face: &Face<'_>) -> BTreeMap<char, GlyphId> {
    let mut map = BTreeMap::new();
    if let Some(cmap) = face.tables().cmap {
        for subtable in cmap.subtables {
            if !subtable.is_unicode() {
                continue;
            }
            subtable.codepoints(|cp| {
                if let Some(ch) = char::from_u32(cp)
                    && let Some(gid) = subtable.glyph_index(cp)
                {
                    map.entry(ch).or_insert(gid);
                }
            });
        }
    }
    map
}

fn pack_face(
    face: &Face<'_>,
    em_per_unit: f32,
    curve_texels: &mut Vec<CurveTexel>,
    band_texels: &mut Vec<BandTexel>,
    glyphs: &mut Vec<GlyphRecord>,
) -> (HashMap<char, u32>, u32) {
    let mut packed_by_gid: HashMap<GlyphId, u32> = HashMap::new();
    let mut pack_gid = |gid: GlyphId| -> u32 {
        if let Some(&idx) = packed_by_gid.get(&gid) {
            return idx;
        }
        let contours = glyph_contours(face, gid, em_per_unit);
        let mut flat_curves = Vec::new();
        for contour in &contours {
            flat_curves.extend_from_slice(contour);
        }
        let empty = flat_curves.is_empty();
        let record = if empty {
            GlyphRecord {
                empty: true,
                ..GlyphRecord::default()
            }
        } else {
            let bands = make_bands(&flat_curves);
            let curve_locs = pack_curves(curve_texels, &contours);
            let location = pack_bands(band_texels, &bands, &curve_locs);
            GlyphRecord {
                quad: bands.tiled,
                bands: bands.transform,
                location,
                band_max_x: (bands.vertical.len() - 1) as u16,
                band_max_y: (bands.horizontal.len() - 1) as u16,
                empty: false,
            }
        };
        let idx = glyphs.len() as u32;
        glyphs.push(record);
        packed_by_gid.insert(gid, idx);
        idx
    };

    // Pack .notdef (GlyphId(0)) first as fallback.
    let fallback = pack_gid(GlyphId(0));

    let codepoints = collect_mapped_codepoints(face);
    let mut lookup = HashMap::with_capacity(codepoints.len());
    for (ch, gid) in codepoints {
        let idx = pack_gid(gid);
        lookup.insert(ch, idx);
    }

    (lookup, fallback)
}

/// Parses and packs the embedded Fira Code Regular and Bold faces into Slug textures.
pub fn build_embedded_fonts() -> FontData {
    build_font_data(FIRA_CODE_REGULAR, FIRA_CODE_BOLD)
}

pub fn build_font_data(regular_bytes: &[u8], bold_bytes: &[u8]) -> FontData {
    let regular_face = Face::parse(regular_bytes, 0).expect("embedded regular font must parse");
    let bold_face = Face::parse(bold_bytes, 0).expect("embedded bold font must parse");

    let regular_em = 1.0 / f32::from(regular_face.units_per_em());
    let bold_em = 1.0 / f32::from(bold_face.units_per_em());

    let advance_units = regular_face
        .glyph_index('M')
        .and_then(|gid| regular_face.glyph_hor_advance(gid))
        .unwrap_or(regular_face.units_per_em() * 3 / 5);

    let upem = f32::from(regular_face.units_per_em());
    let metrics = FontMetrics {
        ascent_em: f32::from(regular_face.ascender()) / upem,
        descent_em: f32::from(regular_face.descender()) / upem,
        line_gap_em: f32::from(regular_face.line_gap()) / upem,
        advance_em: f32::from(advance_units) / upem,
    };

    let mut curve_texels = Vec::new();
    let mut band_texels = Vec::new();
    let mut glyphs = Vec::new();

    let (regular, fallback_regular) = pack_face(
        &regular_face,
        regular_em,
        &mut curve_texels,
        &mut band_texels,
        &mut glyphs,
    );
    let (bold, fallback_bold) = pack_face(&bold_face, bold_em, &mut curve_texels, &mut band_texels, &mut glyphs);

    pad_to_whole_rows(&mut curve_texels);
    pad_to_whole_rows(&mut band_texels);

    FontData {
        metrics,
        curve_texels,
        band_texels,
        glyphs,
        regular,
        bold,
        fallback_regular,
        fallback_bold,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_fira_code_packs_regular_and_bold_with_expected_ui_glyphs() {
        let data = build_embedded_fonts();
        assert_eq!(data.metrics, apus::grid::FIRA_CODE_METRICS);
        assert!(data.glyph_count() > 2000, "expected Regular + Bold mapped glyphs");
        assert_eq!(data.curve_texels.len() % TEXTURE_WIDTH, 0);
        assert_eq!(data.band_texels.len() % TEXTURE_WIDTH, 0);

        for ch in [
            'A', 'g', '0', '←', '↑', '→', '↓', '↔', '⇥', '⏎', '◀', '▸', '·', '×', '…',
        ] {
            let reg = data.glyph(ch, false);
            let bld = data.glyph(ch, true);
            assert!(!reg.empty, "regular glyph {ch:?} should have outline curves");
            assert!(!bld.empty, "bold glyph {ch:?} should have outline curves");
        }
        assert!(data.glyph(' ', false).empty, "space must be empty");
    }
}
