//! The one horizontal position, and the per-frame view derived from it.
//!
//! Two types, split by who owns them. [`Position`] is the shell's single clamped centre coordinate
//! — the only mutable scroll state in the app. [`ViewPosition`] is the immutable snapshot handed to
//! every track during `draw`, derived fresh each frame from that centre and the pane's span.
//!
//! Tracks receive a `ViewPosition` and hold no scroll offset of their own (invariant 10), which is
//! what makes invariant 9 hold: every track draws from the same centre and span, so terminal column
//! N is the same genomic position in all of them. A track that cached its own offset could drift
//! from its siblings by a frame; there is deliberately nowhere to put one.
//!
//! `centre` is the anchor and is never clamped by the derivation — `left` saturates at the origin
//! instead, and `underflow` reports how far it did, so a track can leave that many columns blank
//! rather than shifting its content right. Keeping the anchor unclamped is what lets a width change
//! (a zoom) hold the same coordinate at the centre.
//!
//! Position no longer animates; scroll interpolation was removed 2026-08-06.

use crate::basepair::BasePair;

/// Centre-anchored viewport geometry handed to tracks during drawing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewPosition {
    /// The coordinate held at the viewport centre.
    pub centre: BasePair,
    /// First visible coordinate, saturated at zero.
    pub left: BasePair,
    /// Base-pair span visible across the pane.
    pub width: BasePair,
    /// Base-pair span before contig origin that must remain blank.
    pub underflow: BasePair,
}

impl ViewPosition {
    /// Derives `left` and `underflow` from the anchor and span.
    pub fn new(centre: BasePair, width: BasePair) -> Self {
        let half = width / 2;
        Self {
            centre,
            left: centre.checked_sub(half).unwrap_or(BasePair(0)),
            width,
            underflow: half.checked_sub(centre).unwrap_or(BasePair(0)),
        }
    }
}

/// The shell-owned centre coordinate.
#[derive(Debug, Clone, Copy, Default)]
pub struct Position {
    value: BasePair,
}

impl Position {
    /// The current value.
    pub fn value(&self) -> BasePair {
        self.value
    }

    /// Moves by `delta` (saturating at 0), clamped to `max`.
    pub fn nudge(&mut self, delta: i64, max: BasePair) {
        let target = if delta < 0 {
            self.value
                .checked_sub(BasePair(delta.unsigned_abs()))
                .unwrap_or(BasePair(0))
        } else {
            self.value + BasePair(delta as u64)
        };
        self.value = target.min(max);
    }

    /// Sets an absolute position, clamped to `max`.
    pub fn jump_to(&mut self, target: BasePair, max: BasePair) {
        self.value = target.min(max);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewport_and_position_match_coordinate_algebra() {
        for (centre, width) in [(0, 1), (0, 80), (10, 80), (40, 80), (1_000, 80), (5_000, 160)] {
            let half = width / 2;
            let view = ViewPosition::new(BasePair(centre), BasePair(width));
            assert_eq!(view.centre, BasePair(centre));
            assert_eq!(view.width, BasePair(width));
            assert_eq!(view.left, BasePair(centre.saturating_sub(half)));
            assert_eq!(view.underflow, BasePair(half.saturating_sub(centre)));
        }

        let close = ViewPosition::new(BasePair(5_000), BasePair(80));
        let mid = ViewPosition::new(BasePair(5_000), BasePair(160));
        assert_eq!(close.centre, mid.centre);
        assert_eq!(close.left, BasePair(4_960));
        assert_eq!(mid.left, BasePair(4_920));

        let mut nudged = Position::default();
        nudged.jump_to(BasePair(100), BasePair(1_000));
        nudged.nudge(50, BasePair(1_000));
        assert_eq!(nudged.value(), BasePair(150));

        let mut underflow = Position::default();
        underflow.jump_to(BasePair(10), BasePair(1_000));
        underflow.nudge(-100, BasePair(1_000));
        assert_eq!(underflow.value(), BasePair(0));

        let mut overflow = Position::default();
        overflow.nudge(5_000, BasePair(1_000));
        assert_eq!(overflow.value(), BasePair(1_000));
    }
}
