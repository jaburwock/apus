//! Numeric boundary helpers for base-pair-related input and display.
//!
//! [`parse_with_suffix`] turns the compact values accepted at the command-line boundary into
//! integers: plain integers are preserved, while decimal mantissas may use case-insensitive `k`
//! or `M` suffixes and are rounded after scaling. The vocabulary is deliberately limited to those
//! units; this is input convenience rather than a general human-number parser, so decimals without
//! a suffix and larger units such as `G` are rejected, and suffixes are never emitted on output.
//! [`format_with_commas`] is the complementary plain-integer formatter used by rulers and labels;
//! it groups digits without adding coordinate-base semantics or a unit suffix.
//!
//! This module stays separate from [`crate::basepair::BasePair`] because parsing and grouping are
//! numeric presentation concerns, whereas `BasePair` owns the 0-based coordinate/length model
//! and its sanctioned 1-based display conversions. Parsing maps into a coordinate only at the
//! `BasePair` API boundary; comma formatting can also serve a raw count without falsely wrapping
//! that count in a `BasePair`.

/// Unit suffix `parse_with_suffix` strips from user-typed input (e.g. `"12k"`
/// -> `12_000`). Only used for parsing today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Suffix {
    K,
    M,
}

impl Suffix {
    /// Returns the multiplier.
    fn divisor(self) -> f64 {
        match self {
            Suffix::K => 1_000.0,
            Suffix::M => 1_000_000.0,
        }
    }

    /// Returns accepted suffix characters.
    fn chars(self) -> &'static [char] {
        match self {
            Suffix::K => &['k', 'K'],
            Suffix::M => &['m', 'M'],
        }
    }
}

/// Suffixes accepted by the parser.
const PARSEABLE_SUFFIXES: &[Suffix] = &[Suffix::K, Suffix::M];

/// Formats an integer with comma separators.
pub fn format_with_commas(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Parses an integer with an optional unit suffix.
pub fn parse_with_suffix(input: &str) -> Option<u64> {
    let input = input.trim();
    for &suffix in PARSEABLE_SUFFIXES {
        if let Some(digits) = input.strip_suffix(suffix.chars()) {
            let mantissa: f64 = digits.parse().ok()?;
            let scaled = (mantissa * suffix.divisor()).round();
            return (scaled.is_finite() && scaled >= 0.0 && scaled <= u64::MAX as f64).then_some(scaled as u64);
        }
    }
    input.parse::<u64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffix_parser_accepts_supported_forms_and_rejects_ambiguous_input() {
        assert_eq!(parse_with_suffix("12345"), Some(12_345));
        assert_eq!(parse_with_suffix("12k"), Some(12_000));
        assert_eq!(parse_with_suffix("12K"), Some(12_000));
        assert_eq!(parse_with_suffix("12m"), Some(12_000_000));
        assert_eq!(parse_with_suffix("12M"), Some(12_000_000));
        assert_eq!(parse_with_suffix("1.5k"), Some(1_500));
        assert_eq!(parse_with_suffix("1.123M"), Some(1_123_000));
        assert_eq!(parse_with_suffix("1.1235M"), Some(1_123_500));
        assert_eq!(parse_with_suffix("1.5"), None);
        assert_eq!(parse_with_suffix("abc"), None);
        assert_eq!(parse_with_suffix(""), None);
        assert_eq!(parse_with_suffix("18446744073709551615"), Some(u64::MAX));
        assert_eq!(parse_with_suffix("18446744073709551616"), None);
    }

    #[test]
    fn format_with_commas_groups_by_thousands() {
        assert_eq!(format_with_commas(0), "0");
        assert_eq!(format_with_commas(999), "999");
        assert_eq!(format_with_commas(1_000), "1,000");
        assert_eq!(format_with_commas(1_234_567), "1,234,567");
    }
}
