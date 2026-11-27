//! Paints one pileup lane in explicit compositing order.
//!
//! The painter writes bp-by-lane pixels and glyphs into a caller-owned [`Framebuffer`]. It first
//! lays down the centline and read backgrounds, then the lowest-priority direction marks, the
//! centline highlight, CIGAR-derived signals, and finally close-zoom letters. Later layers replace
//! earlier ones on the same plane, while glyphs with no background preserve the reduced pixel
//! colours underneath them.
//!
//! `LaneCoverage` classifies each read against the resident reference before this module chooses
//! colours, so mismatch and indel rendering stays reference-backed and zoom reduction can colour
//! each half of a wide column independently. The centline arrives as one pixel anchor shared by
//! its highlight; re-deriving either anchor here would recreate the scroll/parity drift this order
//! is intended to make impossible.

use ratatui::style::Color;

use crate::basepair::BasePair;
use crate::residue::BaseResidue;
use crate::setting::ReadColor;
use crate::style::{Alignment, Indel, Reference, Text};
use crate::viewer_app::framebuffer::{Framebuffer, Glyph};
use crate::viewer_app::genome::mod_5mc::Call;
use crate::viewer_app::genome::reference_data::ReferenceLookup;
use crate::viewer_app::genome::settings::GenomeViewerSettings;
use crate::viewer_app::painter::VERTICAL_RULE;

use super::super::mod_5mc_color;
use super::EXPAND_INSERTION_MIN_LEN;
use super::classify::{BaseKind, ExpandedInsertion, LaneCoverage};

/// Base pairs between direction arrows along a read.
///
/// Denominated in bp rather than columns so the marks keep their spacing *within the read* across
/// zoom levels, rather than doubling their genomic stride when a column widens to 2bp.
const ARROW_SPACING: u64 = 10;

/// Parent-read glyph replacing `'I'` on flanks of an expanded insertion (`>= EXPAND_INSERTION_MIN_LEN`).
pub(super) const EXPANDED_INSERTION_MARKER: char = '▼';

/// Returns the tint parity for a genomic base.
pub(super) fn base_parity_tinted(pos: BasePair) -> bool {
    pos.as_u64() % 2 == 1
}

/// Whether `kind` renders as plain read background in the current colour mode.
///
/// In `FiveMc` mode mismatches are suppressed to neutral lane grey, so arrows, centline
/// highlights, and read-base digits treat an unmodified mismatch the same as a match.
fn is_plain_base(kind: BaseKind, show_5mc: bool) -> bool {
    kind == BaseKind::Match || (show_5mc && kind == BaseKind::Mismatch)
}

/// Whether an arrow may occupy the column starting at `bp`.
///
/// Phrased positively — every base the column covers must be a plain unmodified base — so that a
/// `BaseKind` added later suppresses arrows by default instead of being silently drawn over on the
/// day it lands. At 2bp columns this is what keeps an arrow off a mismatch or 5mC call sharing its
/// cell.
fn column_is_plain_match(coverage: &LaneCoverage, bp: BasePair, bp_per_column: u64, show_5mc: bool) -> bool {
    (0..bp_per_column).all(|offset| {
        let bp = bp + BasePair(offset);
        is_plain_base(coverage.kind_at_bp(bp), show_5mc) && coverage.mod_5mc_prob_at_bp(bp).is_none()
    })
}

/// Which per-base marks are legible at the current zoom.
///
/// One decision, not independent bool flags: each field answers how a cell-level mark renders at
/// the current zoom, lane packing, and overlay settings. Grouped because adjacent `bool`
/// parameters are a call site where a transposition compiles and silently paints the wrong frame.
#[derive(Clone, Copy)]
pub(super) struct LaneGlyphs {
    /// Base letters, which need a full cell per base.
    pub(super) letters: bool,
    /// Read-offset digits, gated by the user's setting rather than by zoom.
    pub(super) digits: bool,
    /// Direction arrows.
    pub(super) arrows: bool,
    /// Single-lane reference-skip (`N`) `-` connectors; 2-lane rows use `paint_mid_vertical_skips`.
    pub(super) skips: bool,
    /// Whether large insertions expand into synthetic sub-lanes (switches parent `'I'` to `'▼'`).
    pub(super) expand_insertions: bool,
}

impl LaneGlyphs {
    /// Derives the mark flags from the settings and the current packing.
    ///
    /// `arrows` and `skips` deliberately do not share `letters`' condition: cell-wide marks survive
    /// the 2bp columns of `Mid` horizontal zoom, but not two lanes to a row. The glyph plane is
    /// cell-strided, so at two lanes per row both lanes share one glyph slot and a mark could not
    /// be attributed to one read.
    pub(super) fn new(settings: &GenomeViewerSettings, lanes_per_row: usize, bp_per_column: u64) -> Self {
        Self {
            letters: lanes_per_row == 1 && bp_per_column == 1,
            digits: settings.readbasedigits,
            arrows: settings.readarrows && lanes_per_row == 1,
            skips: lanes_per_row == 1,
            expand_insertions: settings.expandinsertions,
        }
    }
}

/// Paints the centline marker on one lane before any read or expansion signal can overwrite it.
///
/// Its plane depends on the zoom and the two cannot be unified: the pixel plane's horizontal
/// resolution is `bp_per_column`, so at `Close` a pixel reduces to a whole block and a thin rule
/// is only representable as a glyph.
fn paint_centline(fb: &mut Framebuffer, centline_x: Option<usize>, lane_y: usize, bp_per_column: u64) {
    if let Some(cx) = centline_x {
        if bp_per_column >= 2 {
            fb.set_pixel(cx, lane_y, Text::PRIMARY_FG());
        } else {
            fb.set_glyph(
                cx,
                lane_y,
                Glyph {
                    ch: VERTICAL_RULE,
                    fg: Text::PRIMARY_FG(),
                    bg: None,
                    bold: false,
                },
            );
        }
    }
}

/// Paints one lane into the framebuffer and reusable classification storage. Layers run from
/// centline and read background through highlights and CIGAR signals to glyphs.
#[allow(
    clippy::too_many_arguments,
    reason = "the cohesive groups are already bundled (LaneGlyphs, ReferenceLookup); what remains \
              is the lane's own geometry, each value from a different owner"
)]
pub(super) fn paint_one_lane_against(
    fb: &mut Framebuffer,
    lane_reads: &[&super::super::read::Read],
    lane_idx: usize,
    lane_y: usize,
    left: BasePair,
    bp_width: usize,
    bounds_bp: usize,
    content_columns: u16,
    bp_per_column: u64,
    glyphs: LaneGlyphs,
    read_color: ReadColor,
    centline_px: Option<usize>,
    coverage: &mut LaneCoverage,
    mut reference: ReferenceLookup<'_>,
) {
    coverage.reset(content_columns, left, bp_per_column);
    let centline_x = centline_px.filter(|&x| x < bp_width);
    let show_5mc = read_color == ReadColor::FiveMc;

    // The centline goes first so everything after can overwrite it.
    paint_centline(fb, centline_x, lane_y, bp_per_column);

    for read in lane_reads {
        let start_x = read.start.as_u64().saturating_sub(left.as_u64()).min(bounds_bp as u64) as usize;
        let end_x = (read.start + read.length)
            .as_u64()
            .saturating_sub(left.as_u64())
            .min(bounds_bp as u64) as usize;
        let end_x = end_x.min(bp_width);

        // Constant per read: only the tint varies along it, and that stays keyed to absolute
        // genomic position so the banding does not shift under scroll.
        let pair = match read_color {
            ReadColor::Lane | ReadColor::FiveMc => Alignment::pileup_lane_color_pair(lane_idx),
            ReadColor::Direction => Alignment::read_strand_color_pair(read.strand.is_reverse(), lane_idx),
        };

        // Classify first so skipped (`N`) bases leave the plain track background and centline
        // untouched rather than painting filled read pixels underneath the `-` connector.
        coverage.classify_read_against(read, content_columns, reference.reborrow());

        for x in start_x..end_x {
            let bp = left + BasePair(x as u64);
            if coverage.kind_at_bp(bp) == BaseKind::Skip {
                continue;
            }
            fb.set_pixel(x, lane_y, pair.pick(base_parity_tinted(bp)));
            // Blanks the centline rule under a read: a covered base is marked by the highlight
            // instead, and the rule survives only in the gaps between reads.
            fb.clear_glyph(x, lane_y);
        }
    }

    // Arrows sit here, after the background loop that clears glyphs and before every CIGAR layer
    // below, which is what makes them the lowest-priority mark: a mismatch, indel, or letter drawn
    // later simply overwrites one. Coverage is complete for the whole lane by now, so an arrow on
    // one read is also suppressed by an overlapping read's mismatch in the same cell.
    if glyphs.arrows {
        for read in lane_reads {
            let arrow = if read.strand.is_reverse() { '<' } else { '>' };
            let read_end = read.start + read.length;
            // Half a spacing in from the read's own start, so the marks hold a constant offset
            // within the read under scroll, and a read shorter than one full spacing still gets a
            // mark near its middle rather than one jammed against its left edge.
            let mut bp = read.start + BasePair(ARROW_SPACING / 2);

            while bp < read_end {
                if let Some(offset) = bp.checked_sub(left) {
                    // Snap to the start of the containing column before testing or drawing. A
                    // glyph occupies its whole cell, so at 2bp columns an arrow pinned to an odd bp
                    // renders over the even bp beside it — testing from the unsnapped position
                    // would check one base the cell does not cover and miss one it does.
                    let x = (offset.as_u64() / bp_per_column * bp_per_column) as usize;
                    let column_bp = left + BasePair(x as u64);
                    if x < bp_width
                        && x < bounds_bp
                        && column_is_plain_match(coverage, column_bp, bp_per_column, show_5mc)
                    {
                        fb.set_glyph(
                            x,
                            lane_y,
                            Glyph {
                                ch: arrow,
                                fg: Alignment::ARROW_FG(),
                                // Keeps the lane banding and strand colour underneath: an opaque
                                // bg here would punch a flat hole in the read.
                                bg: None,
                                bold: false,
                            },
                        );
                    }
                }
                bp = bp + BasePair(ARROW_SPACING);
            }
        }
    }

    let covered_bp = coverage.covered_bp_range(bounds_bp);
    for x in covered_bp.clone() {
        let bp = left + BasePair(x as u64);
        let kind = coverage.kind_at_bp(bp);

        if centline_x == Some(x) && is_plain_base(kind, show_5mc) {
            fb.set_pixel(x, lane_y, Alignment::highlight_color(lane_idx));
        }

        if let Some(prob) = coverage.mod_5mc_prob_at_bp(bp) {
            fb.set_pixel(x, lane_y, mod_5mc_color(Call::from_prob(prob)));
        }
        match kind {
            BaseKind::Mismatch if !show_5mc => {
                fb.set_pixel(x, lane_y, mismatch_block_color(coverage.base_letter_at_bp(bp)));
            }
            BaseKind::Deletion => {
                fb.set_pixel(x, lane_y, Indel::DELETION());
            }
            BaseKind::InsertionFlank => {
                fb.set_pixel(x, lane_y, Indel::INSERTION());
            }
            _ => {}
        }
    }

    if glyphs.skips {
        let scale = bp_per_column as usize;
        let first_col = covered_bp.start / scale;
        let past_last_col = covered_bp.end.div_ceil(scale);
        let centline_col = centline_x.map(|cx| cx / scale);
        let clip_bp = bounds_bp.min(bp_width);
        for col in first_col..past_last_col {
            // Leave the centline column untouched so the vertical rule (or half-block at Mid)
            // stays visible across the skip.
            if centline_col == Some(col) {
                continue;
            }
            let col_x = col * scale;
            let col_end = (col_x + scale).min(clip_bp);
            if col_end - col_x == scale
                && (col_x..col_end).all(|x| coverage.kind_at_bp(left + BasePair(x as u64)) == BaseKind::Skip)
            {
                fb.set_glyph(
                    col_x,
                    lane_y,
                    Glyph {
                        ch: '-',
                        fg: Text::DIMMED_FG(),
                        bg: None,
                        bold: false,
                    },
                );
            }
        }
    }

    if glyphs.letters {
        for x in covered_bp {
            let bp = left + BasePair(x as u64);
            let kind = coverage.kind_at_bp(bp);
            match kind {
                BaseKind::Mismatch if !show_5mc => {
                    let letter = coverage.base_letter_at_bp(bp);
                    let ch = letter.map_or('?', |b| b.to_ascii_uppercase() as char);
                    fb.set_glyph(
                        x,
                        lane_y,
                        Glyph {
                            ch,
                            fg: Text::ON_DATA_FG(),
                            bg: Some(mismatch_block_color(letter)),
                            bold: false,
                        },
                    );
                }
                BaseKind::Match | BaseKind::Mismatch => {
                    if glyphs.digits
                        && coverage.mod_5mc_prob_at_bp(bp).is_none()
                        && let Some(digit) = coverage.digit_at_bp(bp)
                    {
                        fb.set_glyph(
                            x,
                            lane_y,
                            Glyph {
                                ch: digit,
                                fg: Text::PRIMARY_FG(),
                                bg: None,
                                bold: false,
                            },
                        );
                    }
                }
                BaseKind::Deletion => {
                    fb.set_glyph(
                        x,
                        lane_y,
                        Glyph {
                            ch: 'D',
                            fg: Text::ON_DATA_FG(),
                            bg: Some(Indel::DELETION()),
                            bold: false,
                        },
                    );
                }
                BaseKind::InsertionFlank => {
                    let ch = if glyphs.expand_insertions
                        && coverage
                            .indel_size_at_bp(bp)
                            .is_some_and(|size| size >= EXPAND_INSERTION_MIN_LEN)
                    {
                        EXPANDED_INSERTION_MARKER
                    } else {
                        'I'
                    };
                    fb.set_glyph(
                        x,
                        lane_y,
                        Glyph {
                            ch,
                            fg: Text::ON_DATA_FG(),
                            bg: Some(Indel::INSERTION()),
                            bold: false,
                        },
                    );
                }
                BaseKind::Skip | BaseKind::Uncovered => {}
            }
        }
    }
}

/// Paints one synthetic insertion-expansion sub-lane in the same layer order as read lanes:
/// centline first, solid insertion pixels clipped to `[0, bounds_bp)` and `[0, bp_width)`, and
/// `'I'` letters at `(Close, Close)`.
#[allow(
    clippy::too_many_arguments,
    reason = "matches paint_one_lane_against's lane geometry parameters"
)]
pub(super) fn paint_expansion_lane(
    fb: &mut Framebuffer,
    spans: &[ExpandedInsertion],
    lane_y: usize,
    left: BasePair,
    bp_width: usize,
    bounds_bp: usize,
    bp_per_column: u64,
    glyphs: LaneGlyphs,
    centline_px: Option<usize>,
) {
    let centline_x = centline_px.filter(|&x| x < bp_width);
    paint_centline(fb, centline_x, lane_y, bp_per_column);

    let clip_bp = bounds_bp.min(bp_width) as u64;
    for span in spans {
        let start_x = span.start.as_u64().saturating_sub(left.as_u64()).min(clip_bp) as usize;
        let end_x = (span.start + span.length)
            .as_u64()
            .saturating_sub(left.as_u64())
            .min(clip_bp) as usize;

        for x in start_x..end_x {
            fb.set_pixel(x, lane_y, Indel::INSERTION());
            if glyphs.letters {
                fb.set_glyph(
                    x,
                    lane_y,
                    Glyph {
                        ch: 'I',
                        fg: Text::ON_DATA_FG(),
                        bg: Some(Indel::INSERTION()),
                        bold: false,
                    },
                );
            } else {
                fb.clear_glyph(x, lane_y);
            }
        }
    }
}

/// Paints Braille reference-skip (`N`) connectors for one `(Close, Mid)` cell row after both
/// sub-lanes (`top_y` and `top_y + 1`) have been painted.
///
/// Uses dots 2,5 (`⠒`) for the top lane and dots 7,8 (`⣀`) for the bottom lane (`⣒` when both
/// skip) for uniform 2-dot vertical pitch across lanes and row boundaries. Leaves a cell untouched
/// at the centline or if either sub-lane already painted a pixel (such as when `expandinsertions`
/// shifts logical lane pairs out of visual row parity).
pub(super) fn paint_mid_vertical_skips(
    fb: &mut Framebuffer,
    top_y: usize,
    left: BasePair,
    centline_x: Option<usize>,
    top_skips: &[bool],
    bottom_coverage: Option<&LaneCoverage>,
) {
    for (x, &top_skip) in top_skips.iter().enumerate() {
        if centline_x == Some(x) {
            continue;
        }
        let bot_skip = bottom_coverage.is_some_and(|cov| cov.kind_at_bp(left + BasePair(x as u64)) == BaseKind::Skip);
        let ch = match (top_skip, bot_skip) {
            (true, false) => '⠒',
            (false, true) => '⣀',
            (true, true) => '⣒',
            (false, false) => continue,
        };
        if fb.pixel(x, top_y).is_some() || fb.pixel(x, top_y + 1).is_some() {
            continue;
        }
        fb.set_glyph(
            x,
            top_y,
            Glyph {
                ch,
                fg: Text::DIMMED_FG(),
                bg: None,
                bold: false,
            },
        );
    }
}

fn mismatch_block_color(base: Option<u8>) -> Color {
    match base.and_then(|b| BaseResidue::try_from(b).ok()) {
        Some(residue) => Reference::base_color(residue),
        None => Alignment::UNKNOWN_MISMATCH(),
    }
}
