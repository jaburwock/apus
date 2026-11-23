//! Natural chromosome keys shared by the contig grid and modal contig selector.
//!
//! `chrom_key` turns recognized chromosome spellings into an orderable key and returns `None` for
//! names it cannot classify. Keeping classification here lets both consumers agree on ordering while
//! leaving unrecognized contigs for the caller to handle.

/// Natural chromosome sort key.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ChromKey {
    Numbered(u32),
    X,
    Y,
    M,
}

/// Parses a numbered chromosome, X, Y, or M/MT name.
pub fn chrom_key(name: &str) -> Option<ChromKey> {
    let stripped = name
        .strip_prefix("chr")
        .or_else(|| name.strip_prefix("Chr"))
        .or_else(|| name.strip_prefix("CHR"))
        .unwrap_or(name);
    if let Ok(n) = stripped.parse::<u32>() {
        return Some(ChromKey::Numbered(n));
    }
    match stripped.to_ascii_uppercase().as_str() {
        "X" => Some(ChromKey::X),
        "Y" => Some(ChromKey::Y),
        "M" | "MT" => Some(ChromKey::M),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrom_key_parses_numbered_x_y_m() {
        assert_eq!(chrom_key("chr1"), Some(ChromKey::Numbered(1)));
        assert_eq!(chrom_key("22"), Some(ChromKey::Numbered(22)));
        assert_eq!(chrom_key("chrX"), Some(ChromKey::X));
        assert_eq!(chrom_key("chry"), Some(ChromKey::Y));
        assert_eq!(chrom_key("CHRM"), Some(ChromKey::M));
        assert_eq!(chrom_key("chrMT"), Some(ChromKey::M));
        assert_eq!(chrom_key("scaffold_1"), None);
    }

    #[test]
    fn chrom_key_orders_numbered_before_x_y_m() {
        assert!(ChromKey::Numbered(22) < ChromKey::X);
        assert!(ChromKey::X < ChromKey::Y);
        assert!(ChromKey::Y < ChromKey::M);
    }
}
