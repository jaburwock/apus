//! Concrete settings owned by `GenomeViewer`.
//!
//! The app-global command vocabulary and typed setting values are declared in `crate::setting`;
//! this module stores the genome-owned fields and supplies the generated construction/read/write
//! accessors.
//! `horizontal_zoom` and `feature_filter` are viewer state, but are intentionally not plain fields
//! in the declaration macro's contract.
//!
//! `set` is the single production write path. It preserves the invariant that the centreline
//! read-length and indel-length labels are mutually exclusive; movement values are `BasePair`s,
//! while local-area settings use ordinary counts and ratios.

use super::zoom::HorizontalZoom;
use crate::basepair::BasePair;
use crate::locus::FeatureFilter;
#[cfg(test)]
use crate::setting::Setting;
use crate::setting::{ReadColor, SettingValue};

/// `GenomeViewer`'s movement, overlays, and viewer-only settings.
///
/// Field names must match the `settings!` declaration in `setting.rs`, which is what
/// `setting_accessors!` reads them back through.
///
/// Flat on purpose: nested overlay sub-structs were pure namespacing that cost a hand-written
/// match per accessor. A row writing the wrong field is not caught at compile time; generating
/// the struct from the declaration would catch it, but was rejected because it moved the field
/// list out of this file behind ~125 lines of macro.
pub struct GenomeViewerSettings {
    // Scroll step sizes
    pub(crate) movemin: BasePair,
    pub(crate) movemid: BasePair,
    pub(crate) movefar: BasePair,

    // Alignment overlays
    pub(crate) ruler: bool,
    pub(crate) rulermarks: bool,
    pub(crate) rulernumbers: bool,
    pub(crate) depthindicator: bool,
    pub(crate) coveragetrack: bool,
    pub(crate) allelefraction: u64,
    pub(crate) featurestrack: bool,
    pub(crate) localareatrack: bool,
    pub(crate) localareafeatures: bool,
    pub(crate) localareascale: u64,
    pub(crate) localareareadspans: bool,
    pub(crate) localareamin5mc: u64,
    pub(crate) localareabreakpoints: bool,
    pub(crate) localareabreakangle: u64,
    pub(crate) centnumber: bool,
    pub(crate) centline: bool,
    pub(crate) readbasedigits: bool,
    pub(crate) readarrows: bool,
    pub(crate) readlength: bool,
    pub(crate) indellength: bool,
    pub(crate) expandinsertions: bool,

    // Read colouring
    pub(crate) readcolor: ReadColor,
    pub(crate) colorlegend: bool,

    // Reference overlays
    pub(crate) contigmarkers: bool,
    pub(crate) chromosometrack: bool,
    pub(crate) referencetrack: bool,
    pub(crate) coordinatetrack: bool,
    pub(crate) sequencerow: bool,

    // Glyph vocabulary
    pub(crate) octants: bool,
    pub(crate) dividerbox: bool,

    // Set through registry commands, not `settings!`: zoom needs `apply_zoom`, the filter takes edits
    pub(crate) horizontal_zoom: HorizontalZoom,
    pub(crate) feature_filter: FeatureFilter,
}

setting_accessors!(
    GenomeViewerSettings,
    Genome,
    [
        horizontal_zoom: HorizontalZoom::default(),
        feature_filter: FeatureFilter::none(),
    ]
);

impl GenomeViewerSettings {
    /// Applies a resolved setting, including mutually exclusive overlays.
    pub(crate) fn set(&mut self, value: SettingValue) -> bool {
        match value {
            SettingValue::Readlength(true) => {
                self.set_field(SettingValue::Readlength(true));
                self.set_field(SettingValue::Indellength(false));
                true
            }
            SettingValue::Indellength(true) => {
                self.set_field(SettingValue::Indellength(true));
                self.set_field(SettingValue::Readlength(false));
                true
            }
            plain => self.set_field(plain),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_name_resolves_to_nothing() {
        assert_eq!(Setting::from_name("nosuchsetting"), None);
        assert_eq!(Setting::from_name(""), None);
    }

    #[test]
    fn enabling_either_centreline_label_disables_the_other() {
        let mut settings = GenomeViewerSettings::new();
        settings.set(SettingValue::Readlength(true));
        assert!(settings.readlength && !settings.indellength);

        settings.set(SettingValue::Indellength(true));
        assert!(settings.indellength && !settings.readlength);

        settings.set(SettingValue::Indellength(false));
        assert!(!settings.indellength && !settings.readlength);
    }

    #[test]
    fn expandinsertions_setting_toggles_its_field() {
        let mut settings = GenomeViewerSettings::new();
        assert!(settings.set(SettingValue::Expandinsertions(true)));
        assert!(settings.expandinsertions);
        assert!(settings.set(SettingValue::Expandinsertions(false)));
        assert!(!settings.expandinsertions);
    }
}
