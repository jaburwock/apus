//! 0-based coordinates and bp-denominated counts; only coordinates gain one
//! in `to_display`.

mod numfmt;

// Re-exported alone: grouping digits is not a bp concern, and callers with a
// plain count (a VCF record number) would otherwise have to wrap it in a
// `BasePair` to reach the formatter, asserting a unit it does not have
// (invariant 2). The rest of `numfmt` is bp parsing and stays private.
pub use numfmt::format_with_commas;

/// A base-pair coordinate or length, stored 0-based; `kind` affects display only.
#[derive(Debug, Clone, Copy)]
pub struct BasePair {
    value: u64,
    kind: BpKind,
}

/// Display-only metadata: coordinates gain one, lengths do not.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
enum BpKind {
    #[default]
    Coord,
    Length,
}

// `kind` is display metadata, so identity and ordering use `value` alone.
impl PartialEq for BasePair {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl Eq for BasePair {}

impl std::hash::Hash for BasePair {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.value.hash(state);
    }
}

impl PartialOrd for BasePair {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for BasePair {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.value.cmp(&other.value)
    }
}

impl Default for BasePair {
    fn default() -> Self {
        BasePair::new(0)
    }
}

/// Shorthand for `BasePair::new(value)`.
#[allow(non_snake_case)]
pub const fn BasePair(value: u64) -> BasePair {
    BasePair::new(value)
}

impl BasePair {
    /// Constructs a coordinate or delta.
    pub const fn new(value: u64) -> BasePair {
        BasePair {
            value,
            kind: BpKind::Coord,
        }
    }

    /// Constructs a length that is not shifted by `to_display`.
    pub fn length(value: u64) -> BasePair {
        BasePair {
            value,
            kind: BpKind::Length,
        }
    }

    /// Parses user text with an optional unit suffix.
    pub fn parse(input: &str) -> Option<BasePair> {
        numfmt::parse_with_suffix(input).map(BasePair::new)
    }

    /// Formats as a comma-separated integer.
    pub fn format_with_commas(&self) -> String {
        numfmt::format_with_commas(self.value)
    }

    /// Checked subtraction without saturation.
    pub fn checked_sub(self, rhs: BasePair) -> Option<BasePair> {
        self.value.checked_sub(rhs.value).map(BasePair::new)
    }

    /// Divides by `rhs`, rounding up.
    pub fn div_ceil(self, rhs: BasePair) -> BasePair {
        BasePair::new(self.value.div_ceil(rhs.value))
    }

    /// Iterates bp values from `self` up to `end` by `step`.
    pub fn step_to(self, end: BasePair, step: BasePair) -> impl Iterator<Item = BasePair> {
        (self.value..end.value).step_by(step.value as usize).map(BasePair::new)
    }

    /// Unwraps to the inner `u64`.
    pub fn as_u64(self) -> u64 {
        self.value
    }

    /// Converts to `f64` for fractional calculations.
    pub fn as_f64(self) -> f64 {
        self.value as f64
    }

    /// Converts a coordinate to 1-based display; lengths are unchanged.
    pub fn to_display(self) -> u64 {
        match self.kind {
            BpKind::Length => self.value,
            BpKind::Coord => self.value + 1,
        }
    }

    /// Converts a 1-based display value to a 0-based coordinate.
    pub fn from_display(value: u64) -> BasePair {
        BasePair::new(value.saturating_sub(1))
    }

    /// Converts an external 1-based coordinate to 0-based.
    pub fn from_1based(one_based: u64) -> BasePair {
        BasePair::new(one_based.saturating_sub(1))
    }
}

impl std::ops::Rem<BasePair> for BasePair {
    type Output = BasePair;

    fn rem(self, rhs: BasePair) -> BasePair {
        BasePair::new(self.value % rhs.value)
    }
}

impl PartialEq<u64> for BasePair {
    fn eq(&self, other: &u64) -> bool {
        self.value == *other
    }
}

impl PartialOrd<u64> for BasePair {
    fn partial_cmp(&self, other: &u64) -> Option<std::cmp::Ordering> {
        self.value.partial_cmp(other)
    }
}

impl From<u64> for BasePair {
    fn from(value: u64) -> Self {
        BasePair::new(value)
    }
}

impl From<u16> for BasePair {
    fn from(value: u16) -> Self {
        BasePair::new(u64::from(value))
    }
}

impl From<BasePair> for u64 {
    fn from(bp: BasePair) -> Self {
        bp.as_u64()
    }
}

impl std::ops::Add<BasePair> for BasePair {
    type Output = BasePair;

    fn add(self, rhs: BasePair) -> BasePair {
        BasePair::new(self.value.saturating_add(rhs.value))
    }
}

impl std::ops::Sub<BasePair> for BasePair {
    type Output = BasePair;

    fn sub(self, rhs: BasePair) -> BasePair {
        BasePair::new(self.value.saturating_sub(rhs.value))
    }
}

impl std::ops::Div<u64> for BasePair {
    type Output = BasePair;

    fn div(self, rhs: u64) -> BasePair {
        BasePair::new(self.value / rhs)
    }
}

impl std::ops::Mul<BasePair> for BasePair {
    type Output = BasePair;

    fn mul(self, rhs: BasePair) -> BasePair {
        BasePair::new(self.value.saturating_mul(rhs.value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_basepairs_hash_equally_regardless_of_kind() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let hash_of = |bp: BasePair| {
            let mut hasher = DefaultHasher::new();
            bp.hash(&mut hasher);
            hasher.finish()
        };

        let coord = BasePair(500);
        let length = BasePair::length(500);
        assert_eq!(coord, length, "kind must not affect equality");
        assert_eq!(
            hash_of(coord),
            hash_of(length),
            "equal values must hash equally or they break HashMap/HashSet keying"
        );

        let mut set = std::collections::HashSet::new();
        set.insert(coord);
        assert!(
            set.contains(&length),
            "a HashSet keyed on a Coord must find the equal Length"
        );
        assert_eq!(set.len(), 1, "inserting both must not yield two entries");
        set.insert(length);
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn coordinate_conversions_round_trip_across_boundaries() {
        use rand::{RngExt, SeedableRng, rngs::StdRng};

        let mut rng = StdRng::seed_from_u64(0xBA5E_0001);
        let mut positions = vec![0, 1, 42, u64::MAX - 1];
        positions.extend((0..128).map(|_| rng.random_range(0..u64::MAX)));

        for internal in positions {
            let bp = BasePair(internal);
            let one_based = bp.to_display();
            assert_eq!(one_based, internal + 1);
            assert_eq!(BasePair::from_display(one_based), bp);
            assert_eq!(BasePair::from_1based(one_based), bp);
            assert_eq!(BasePair::from_1based(one_based).to_display(), one_based);
        }

        assert_eq!(BasePair::from_display(0), BasePair(0));
        assert_eq!(BasePair::from_1based(0), BasePair(0));

        for length in [0, 1, 42, u64::MAX] {
            assert_eq!(BasePair::length(length).to_display(), length);
        }
    }
}
