//! Horizontal and alignment-only vertical zoom levels for genome rendering.
//!
//! `Close` means zoomed in and `Mid` means zoomed out: horizontal `Mid` packs multiple bp into a
//! terminal column, while vertical `Mid` packs multiple pileup lanes into a terminal row.
//! Horizontal zoom is shared by the genome's full-width detail renderers; vertical zoom remains
//! state owned by `AlignmentTrack` because no other track has pileup lanes.
//!
//! The two axes are coupled by one deliberate rendering limitation: `(Mid, Mid)` would require a
//! four-way cell reduction that terminal cells cannot represent with only foreground and
//! background colours. `constrain_vzoom` therefore adjusts only vertical zoom, and `LOD_SEQUENCE`
//! lists the three legal combined states explicitly.
//!
//! `step_lod` walks that explicit sequence with saturation. It is not a computed Cartesian
//! product: the final transition changes both axes. Keep this state machine small and inspectable
//! if another zoom level is added.

/// Shared horizontal render zoom; all full-width tracks map columns identically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HorizontalZoom {
    #[default]
    Close,
    Mid,
}

impl HorizontalZoom {
    /// How many bp one screen column covers at this zoom level.
    pub fn bp_per_column(self) -> u64 {
        match self {
            HorizontalZoom::Close => 1,
            HorizontalZoom::Mid => 2,
        }
    }
}

/// Alignment lanes packed into one terminal row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VerticalZoom {
    #[default]
    Close,
    Mid,
}

impl VerticalZoom {
    /// How many pileup lanes one terminal row packs at this zoom level.
    pub fn lanes_per_row(self) -> usize {
        match self {
            VerticalZoom::Mid => 2,
            VerticalZoom::Close => 1,
        }
    }
}

/// `(Mid, Mid)` is not renderable; keep horizontal zoom and clamp vertical.
pub(super) fn constrain_vzoom(h: HorizontalZoom, v: VerticalZoom) -> VerticalZoom {
    match (h, v) {
        (HorizontalZoom::Mid, VerticalZoom::Mid) => VerticalZoom::Close,
        (_, v) => v,
    }
}

/// Explicit legal order; the final transition changes both axes to avoid `(Mid, Mid)`.
const LOD_SEQUENCE: [(HorizontalZoom, VerticalZoom); 3] = [
    (HorizontalZoom::Close, VerticalZoom::Close),
    (HorizontalZoom::Close, VerticalZoom::Mid),
    (HorizontalZoom::Mid, VerticalZoom::Close),
];

/// Steps through legal zoom states, saturating at either end.
pub(super) fn step_lod(h: HorizontalZoom, v: VerticalZoom, out: bool) -> (HorizontalZoom, VerticalZoom) {
    let current = LOD_SEQUENCE.iter().position(|&s| s == (h, v)).unwrap_or(0);
    let next = if out {
        (current + 1).min(LOD_SEQUENCE.len() - 1)
    } else {
        current.saturating_sub(1)
    };
    LOD_SEQUENCE[next]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constrain_vzoom_normalizes_every_zoom_pair() {
        for (horizontal, vertical, expected) in [
            (HorizontalZoom::Close, VerticalZoom::Close, VerticalZoom::Close),
            (HorizontalZoom::Close, VerticalZoom::Mid, VerticalZoom::Mid),
            (HorizontalZoom::Mid, VerticalZoom::Close, VerticalZoom::Close),
            (HorizontalZoom::Mid, VerticalZoom::Mid, VerticalZoom::Close),
        ] {
            assert_eq!(
                constrain_vzoom(horizontal, vertical),
                expected,
                "{horizontal:?}/{vertical:?}"
            );
        }
    }

    #[test]
    fn lod_sequence_traverses_legal_states_and_saturates() {
        for (h, v) in LOD_SEQUENCE {
            assert_eq!(constrain_vzoom(h, v), v, "{h:?}/{v:?} is not a legal LOD state");
        }
        for pair in LOD_SEQUENCE.windows(2) {
            assert_eq!(step_lod(pair[0].0, pair[0].1, true), pair[1]);
            assert_eq!(step_lod(pair[1].0, pair[1].1, false), pair[0]);
        }
        let first = LOD_SEQUENCE[0];
        let last = *LOD_SEQUENCE.last().expect("sequence is non-empty");
        assert_eq!(step_lod(first.0, first.1, false), first);
        assert_eq!(step_lod(last.0, last.1, true), last);
    }
}
