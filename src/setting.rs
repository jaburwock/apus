//! The command-facing vocabulary for app-global settings.
//!
//! This root leaf sits below `command` and above concrete storage in
//! each viewer: it declares names, aliases, value kinds, owners, defaults, and typed payloads
//! without importing viewer state. The `settings!` declaration is therefore the contract that
//! parsing, help, and viewer application must agree on, while field ownership remains with the
//! viewers.
//!
//! The bus is stateless: resolution produces one [`SettingValue`] and the router broadcasts it;
//! generated owner accessors accept only their rows. The macro rejects duplicate spellings,
//! reversed bounds, and bounded defaults outside those bounds at compile time. Bp-valued settings
//! retain `BasePair` so unit conversion cannot disappear into command parsing. `ReadColor` lives
//! here for the same dependency reason: moving it into a viewer would reintroduce the
//! `command` ↔ `viewer_app` cycle. Cross-field behavior stays with the owning settings struct.

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::basepair::BasePair;

/// The type a setting's value parses as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingKind {
    Bool,
    BasePair,
    BoundedInteger { min: u64, max: u64 },
    ReadColor,
}

/// What a read's background colour encodes in the alignment pileup.
///
/// Lives here rather than with the genome viewer's other setting types because
/// [`SettingValue`] must carry it, and `setting` is a root leaf that `command` depends on —
/// reaching into `viewer_app` for the type would re-close the `command` ↔ `viewer_app` cycle.
/// No `Default` impl: the starting mode is stated once in the setting declaration. A type-level
/// default would be a second declaration of the same fact, free to drift from the bus default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadColor {
    /// The lane-alternating greys — colour carries no fact about the read.
    Lane,
    /// Purple for forward reads, orange for reverse.
    Direction,
    /// Per-base 5mC probabilities at CpGs.
    FiveMc,
}

/// The viewer family that owns a declared setting.
///
/// Routing does not inspect this: every setting is broadcast, and generated viewer accessors
/// decide whether to accept it. The owner is declaration and help-grouping metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingOwner {
    Genome,
    Vcf,
}

impl SettingOwner {
    pub(crate) const ALL: [Self; 2] = [Self::Genome, Self::Vcf];

    pub(crate) const fn help_heading(self) -> &'static str {
        match self {
            Self::Genome => "Genome settings",
            Self::Vcf => "VCF settings",
        }
    }
}

impl ReadColor {
    pub(crate) fn next(self) -> Self {
        match self {
            Self::Lane => Self::Direction,
            Self::Direction => Self::FiveMc,
            Self::FiveMc => Self::Lane,
        }
    }

    /// Every variant with the name `:set readcolor` takes, which is also the round-trip oracle.
    pub(crate) const ALL: [(&'static str, Self); 3] = [
        ("lane", Self::Lane),
        ("direction", Self::Direction),
        ("5mc", Self::FiveMc),
    ];

    /// Resolves a value name, case-insensitively.
    pub(crate) fn parse(arg: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(arg))
            .map(|(_, mode)| *mode)
    }

    /// The canonical spelling of this mode.
    pub(crate) fn name(self) -> &'static str {
        Self::ALL
            .iter()
            .find(|(_, mode)| *mode == self)
            .map(|(name, _)| *name)
            .expect("every variant is declared in ALL")
    }
}

/// One `:set`-able name's declaration.
pub(crate) struct SettingSpec {
    pub(crate) key: Setting,
    /// The setting's own name, as `:set <name>` takes it.
    pub(crate) name: &'static str,
    /// Alternative spellings that resolve to the same setting, for `-our`/`-or` pairs. These are
    /// spellings rather than settings, so `:help` lists only [`Self::name`].
    pub(crate) aliases: &'static [&'static str],
    /// The full command line that reaches it — what `:help` lists.
    pub(crate) set_name: &'static str,
    pub(crate) owner: SettingOwner,
    #[cfg(test)]
    pub(crate) kind: SettingKind,
    #[cfg(test)]
    pub(crate) default: fn() -> SettingValue,
    pub(crate) description: &'static str,
}

/// Declares the app-global `:set` vocabulary — one row per setting, giving its variant, field,
/// command-line spelling, kind, owner, default, and help text.
///
/// Emits `Setting`, `SettingValue`, the spec table, and a `setting_accessors!` macro each owning
/// viewer invokes for its struct.
macro_rules! settings {
    (@kind Bool) => { SettingKind::Bool };
    (@kind BasePair) => { SettingKind::BasePair };
    (@kind BoundedInteger($min:literal, $max:literal)) => {
        SettingKind::BoundedInteger { min: $min, max: $max }
    };
    (@kind ReadColor) => { SettingKind::ReadColor };
    (@assert_kind Bool) => {};
    (@assert_kind BasePair) => {};
    (@assert_kind BoundedInteger($min:literal, $max:literal)) => {
        assert!($min <= $max, "bounded-integer setting has reversed bounds");
    };
    (@assert_kind ReadColor) => {};
    (@assert_default Bool, $default:expr) => {};
    (@assert_default BasePair, $default:expr) => {};
    (@assert_default BoundedInteger($min:literal, $max:literal), $default:expr) => {
        assert!(
            $min <= $default && $default <= $max,
            "bounded-integer setting default is outside its bounds"
        );
    };
    (@assert_default ReadColor, $default:expr) => {};
    (@build_bool $variant:ident, [Bool], $value:expr) => {
        Some(SettingValue::$variant($value))
    };
    (@build_bool $variant:ident, [$($kind:tt)*], $value:expr) => {
        None
    };
    (@build_basepair $variant:ident, [BasePair], $value:expr) => {
        Some(SettingValue::$variant($value))
    };
    (@build_basepair $variant:ident, [$($kind:tt)*], $value:expr) => {
        None
    };
    (@build_bounded_integer $variant:ident, [BoundedInteger($min:literal, $max:literal)], $value:expr) => {
        ($min..=$max).contains(&$value).then(|| SettingValue::$variant($value))
    };
    (@build_bounded_integer $variant:ident, [$($kind:tt)*], $value:expr) => {
        None
    };
    (@build_readcolor $variant:ident, [ReadColor], $value:expr) => {
        Some(SettingValue::$variant($value))
    };
    (@build_readcolor $variant:ident, [$($kind:tt)*], $value:expr) => {
        None
    };
    (@as_bool $variant:ident, [Bool], $value:expr) => {
        Some(*$value)
    };
    (@as_bool $variant:ident, [$($kind:tt)*], $value:expr) => {{
        let _ = $value;
        None
    }};
    (@as_integer $variant:ident, [BoundedInteger($min:literal, $max:literal)], $value:expr) => {
        Some(u64::from(*$value))
    };
    (@as_integer $variant:ident, [$($kind:tt)*], $value:expr) => {{
        let _ = $value;
        None
    }};
    (@command_arg Bool, $value:expr) => { $value.to_string() };
    (@command_arg BasePair, $value:expr) => { $value.as_u64().to_string() };
    (@command_arg BoundedInteger($min:literal, $max:literal), $value:expr) => { $value.to_string() };
    (@command_arg ReadColor, $value:expr) => { $value.name().to_string() };

    ($($variant:ident => $field:ident, $name:literal, $aliases:expr,
        $kind:ident $(($min:literal, $max:literal))?, $ty:ty, $owner:ident, $default:expr,
        $description:literal;)*) => {
        /// Every app-global `:set`-able setting.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub(crate) enum Setting {
            $($variant,)*
        }

        /// A setting paired with its typed value.
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub enum SettingValue {
            $($variant($ty),)*
        }

        impl SettingValue {
            pub(crate) fn key(&self) -> Setting {
                match self {
                    $(SettingValue::$variant(_) => Setting::$variant,)*
                }
            }

            pub(crate) fn as_bool(&self) -> Option<bool> {
                match self {
                    $(SettingValue::$variant(value) =>
                        settings!(@as_bool $variant, [$kind $(($min, $max))?], value),)*
                }
            }

            pub(crate) fn from_bool(key: Setting, value: bool) -> Option<Self> {
                match key {
                    $(Setting::$variant =>
                        settings!(@build_bool $variant, [$kind $(($min, $max))?], value),)*
                }
            }

            pub(crate) fn from_basepair(key: Setting, value: BasePair) -> Option<Self> {
                match key {
                    $(Setting::$variant =>
                        settings!(@build_basepair $variant, [$kind $(($min, $max))?], value),)*
                }
            }

            pub(crate) fn from_bounded_integer(key: Setting, value: u64) -> Option<Self> {
                match key {
                    $(Setting::$variant => settings!(
                        @build_bounded_integer $variant, [$kind $(($min, $max))?], value
                    ),)*
                }
            }

            pub(crate) fn from_read_color(key: Setting, value: ReadColor) -> Option<Self> {
                match key {
                    $(Setting::$variant =>
                        settings!(@build_readcolor $variant, [$kind $(($min, $max))?], value),)*
                }
            }

            /// The value as an integer, for the bounded-integer settings and no others.
            ///
            /// Generated from each setting's declared kind, so a new bounded setting is covered
            /// the moment it is declared rather than when someone remembers to list it.
            #[cfg(test)]
            pub(crate) fn as_integer(&self) -> Option<u64> {
                match self {
                    $(SettingValue::$variant(v) =>
                        settings!(@as_integer $variant, [$kind $(($min, $max))?], v),)*
                }
            }

            pub(crate) fn command_name(&self) -> &'static str {
                match self {
                    $(SettingValue::$variant(_) => concat!("set ", $name),)*
                }
            }

            pub(crate) fn command_arg(&self) -> String {
                match self {
                    $(SettingValue::$variant(value) =>
                        settings!(@command_arg $kind $(($min, $max))?, value),)*
                }
            }
        }

        impl Setting {
            /// Returns setting specifications in declaration order.
            pub(crate) fn specs() -> &'static [SettingSpec] {
                static SPECS: &[SettingSpec] = &[$(SettingSpec {
                    key: Setting::$variant,
                    name: $name,
                    aliases: $aliases,
                    set_name: concat!("set ", $name),
                    owner: SettingOwner::$owner,
                    #[cfg(test)]
                    kind: settings!(@kind $kind $(($min, $max))?),
                    #[cfg(test)]
                    default: || SettingValue::$variant($default),
                    description: $description,
                }),*];
                SPECS
            }

            /// The type this setting's value parses as.
            pub(crate) fn kind(self) -> SettingKind {
                match self {
                    $(Setting::$variant => settings!(@kind $kind $(($min, $max))?),)*
                }
            }
        }

        const _: () = assert!(
            all_names_unique(&[$($name,)*], &[$($aliases,)*]),
            "duplicate setting name or alias declared"
        );

        const _: () = {
            $(settings!(@assert_kind $kind $(($min, $max))?);)*
            $(settings!(@assert_default $kind $(($min, $max))?, $default);)*
        };

        /// Generates one owner's construction and typed accessors over only its declared fields.
        /// Extra fields are viewer state that lives on the same flat struct but is not plain
        /// `:set` vocabulary.
        macro_rules! setting_accessors {
            ($self_ty:ident, Genome, $extra_fields:tt) => {
                settings!(@collect_genome $self_ty, $extra_fields [];
                    $([$variant, $field, $ty, $owner, $default])*
                );
            };
            ($self_ty:ident, Vcf, $extra_fields:tt) => {
                settings!(@collect_vcf $self_ty, $extra_fields [];
                    $([$variant, $field, $ty, $owner, $default])*
                );
            };
        }
    };

    (@collect_genome $self_ty:ident, [$($extra:tt)*] [$($selected:tt)*];) => {
        settings!(@emit_accessors $self_ty, [$($extra)*] $($selected)*);
    };
    (@collect_genome $self_ty:ident, [$($extra:tt)*] [$($selected:tt)*];
        [$variant:ident, $field:ident, $ty:ty, Genome, $default:expr] $($rest:tt)*) => {
        settings!(@collect_genome $self_ty, [$($extra)*]
            [$($selected)* [$variant, $field, $ty, $default]]; $($rest)*);
    };
    (@collect_genome $self_ty:ident, [$($extra:tt)*] [$($selected:tt)*];
        [$variant:ident, $field:ident, $ty:ty, Vcf, $default:expr] $($rest:tt)*) => {
        settings!(@collect_genome $self_ty, [$($extra)*] [$($selected)*]; $($rest)*);
    };
    (@collect_vcf $self_ty:ident, [$($extra:tt)*] [$($selected:tt)*];) => {
        settings!(@emit_accessors $self_ty, [$($extra)*] $($selected)*);
    };
    (@collect_vcf $self_ty:ident, [$($extra:tt)*] [$($selected:tt)*];
        [$variant:ident, $field:ident, $ty:ty, Vcf, $default:expr] $($rest:tt)*) => {
        settings!(@collect_vcf $self_ty, [$($extra)*]
            [$($selected)* [$variant, $field, $ty, $default]]; $($rest)*);
    };
    (@collect_vcf $self_ty:ident, [$($extra:tt)*] [$($selected:tt)*];
        [$variant:ident, $field:ident, $ty:ty, Genome, $default:expr] $($rest:tt)*) => {
        settings!(@collect_vcf $self_ty, [$($extra)*] [$($selected)*]; $($rest)*);
    };
    (@emit_accessors $self_ty:ident, [$($extra_field:ident: $extra_default:expr,)*]
        $([$variant:ident, $field:ident, $ty:ty, $default:expr])*) => {
        impl $self_ty {
            fn set_field(&mut self, value: $crate::setting::SettingValue) -> bool {
                $(if let $crate::setting::SettingValue::$variant(value) = &value {
                    self.$field = value.clone();
                    return true;
                })*
                false
            }

            /// Every declared setting's current value, for `:help` to display.
            pub(crate) fn values(&self) -> Vec<$crate::setting::SettingValue> {
                vec![$($crate::setting::SettingValue::$variant(self.$field.clone()),)*]
            }

            pub fn new() -> Self {
                Self {
                    $($field: $default,)*
                    $($extra_field: $extra_default,)*
                }
            }
        }

        impl Default for $self_ty {
            fn default() -> Self {
                Self::new()
            }
        }
    };
}

/// Whether every declared setting name and alias is distinct across both lists.
///
/// Const-evaluated so a duplicate is a build failure: names and aliases resolve through one
/// `HashMap`, where a collision would silently shadow the earlier setting. Aliases are checked
/// against names too — an alias colliding with a real setting is the worse version of that bug,
/// since it makes a working command resolve elsewhere.
const fn all_names_unique(names: &[&str], alias_lists: &[&[&str]]) -> bool {
    let mut left = 0;
    while left < names.len() {
        let mut right = left + 1;
        while right < names.len() {
            if names_equal(names[left], names[right]) {
                return false;
            }
            right += 1;
        }
        left += 1;
    }

    // Every alias against every name, then against every other alias.
    let mut list = 0;
    while list < alias_lists.len() {
        let mut index = 0;
        while index < alias_lists[list].len() {
            let alias = alias_lists[list][index];
            let mut name = 0;
            while name < names.len() {
                if names_equal(alias, names[name]) {
                    return false;
                }
                name += 1;
            }

            let mut other_list = list;
            while other_list < alias_lists.len() {
                let mut other = if other_list == list { index + 1 } else { 0 };
                while other < alias_lists[other_list].len() {
                    if names_equal(alias, alias_lists[other_list][other]) {
                        return false;
                    }
                    other += 1;
                }
                other_list += 1;
            }
            index += 1;
        }
        list += 1;
    }
    true
}

/// `str` equality, which `==` cannot do in a const context.
const fn names_equal(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let (left, right) = (left.as_bytes(), right.as_bytes());
    let mut index = 0;
    while index < left.len() {
        if left[index] != right[index] {
            return false;
        }
        index += 1;
    }
    true
}

settings! {
    // Scroll step sizes
    Movemin => movemin, "movemin", &[], BasePair, BasePair, Genome, BasePair(1),
        "minimum bp moved per scroll step";
    Movemid => movemid, "movemid", &[], BasePair, BasePair, Genome, BasePair(50),
        "medium bp moved per scroll step (n/p)";
    Movefar => movefar, "movefar", &[], BasePair, BasePair, Genome, BasePair(500),
        "long bp moved per scroll step (N/P)";

    // Alignment overlays
    Ruler => ruler, "ruler", &[], Bool, bool, Genome, true, "show the coordinate ruler";
    Rulermarks => rulermarks, "rulermarks", &[], Bool, bool, Genome, true, "show ruler tick marks";
    Rulernumbers => rulernumbers, "rulernumbers", &[], Bool, bool, Genome, true, "show ruler tick numbers";
    Depthindicator => depthindicator, "depthindicator", &[], Bool, bool, Genome, true,
        "show lane numbers over the read pileup";
    Coveragetrack => coveragetrack, "coveragetrack", &[], Bool, bool, Genome, true,
        "show the coverage bars above the read pileup";
    Allelefraction => allelefraction, "allelefraction", &[], BoundedInteger(1, 100), u64, Genome, 20,
        "minimum non-reference percentage that colours a coverage bar (1-100)";
    Featurestrack => featurestrack, "featurestrack", &[], Bool, bool, Genome, false,
        "show the loaded features under the coordinate ruler";
    Localareatrack => localareatrack, "localareatrack", &[], Bool, bool, Genome, true,
        "show the local-area context track";
    Localareafeatures => localareafeatures, "localareafeatures", &[], Bool, bool, Genome, false,
        "show the loaded features under the local-area track";
    Localareascale => localareascale, "localareascale", &[], BoundedInteger(2, 10_000), u64, Genome, 500,
        "viewport-span multiplier for the local-area track (2-10000)";
    Localareareadspans => localareareadspans, "localareareadspans", &[], Bool, bool, Genome, false,
        "shade local-area coverage contributed by reads spanning the pileup centreline";
    Localareamin5mc => localareamin5mc, "localareamin5mc", &[], BoundedInteger(1, 10_000), u64, Genome, 10,
        "minimum reference CpG sites per local-area bin to shade 5mC (1-10000)";
    Localareabreakpoints => localareabreakpoints, "localareabreakpoints", &[], Bool, bool, Genome, true,
        "recolour local-area coverage where depth changes steeply";
    // The upper bound is not a free choice: the local-area chart fits a least-squares line over
    // a fixed window, whose width caps the normalised slope a step can reach. 45 is the ceiling
    // for the width that chart uses, so the whole range stays reachable instead of going inert
    // above some interior point. `local_area::chart` asserts the two remain in agreement.
    // Below the ~12 degrees a 2:1 copy-number step produces, so a single-copy change is caught.
    // Tuned down from 10 against real coverage, which left real steps unmarked.
    Localareabreakangle => localareabreakangle, "localareabreakangle", &[], BoundedInteger(1, 45), u64, Genome, 7,
        "local-area breakpoint sensitivity as a depth-gradient angle in degrees (1-45; 45 is a total dropout)";
    Centnumber => centnumber, "centnumber", &[], Bool, bool, Genome, true, "show the centreline coordinate number";
    Centline => centline, "centline", &[], Bool, bool, Genome, true, "show the centreline marker";
    Readbasedigits => readbasedigits, "readbasedigits", &[], Bool, bool, Genome, false,
        "show read-offset digits on matched bases";
    Readarrows => readarrows, "readarrows", &[], Bool, bool, Genome, true, "show read direction arrows";
    Readlength => readlength, "readlength", &[], Bool, bool, Genome, false,
        "show read length at the centreline (disables indellength)";
    Indellength => indellength, "indellength", &[], Bool, bool, Genome, true,
        "show indel size at the centreline (disables readlength)";
    Expandinsertions => expandinsertions, "expandinsertions", &[], Bool, bool, Genome, false,
        "expand large insertions into synthetic rows beneath their reads";

    // Read colouring
    // The one statement of the starting mode — `ReadColor` deliberately has no `Default` impl.
    Readcolor => readcolor, "readcolor", &["readcolour"], ReadColor, ReadColor, Genome, ReadColor::Lane,
        "what the read background encodes: lane, direction, or 5mc";
    Colorlegend => colorlegend, "colorlegend", &["colourlegend", "legend"], Bool, bool, Genome, true,
        "show the colour legend overlay";

    // Reference overlays
    Contigmarkers => contigmarkers, "contigmarkers", &[], Bool, bool, Genome, true,
        "show the contig grid";
    Chromosometrack => chromosometrack, "chromosometrack", &[], Bool, bool, Genome, true,
        "show the whole-chromosome overview";
    Referencetrack => referencetrack, "referencetrack", &[], Bool, bool, Genome, true,
        "show the reference track";
    Coordinatetrack => coordinatetrack, "coordinatetrack", &[], Bool, bool, Genome, true,
        "show the coordinate track";
    Sequencerow => sequencerow, "sequencerow", &[], Bool, bool, Genome, true, "show the reference sequence row";

    // Glyph vocabulary
    // Octants are Unicode 16 (2024); many terminal fonts still lack them and render tofu. Off by
    // default so a fresh install is always legible, and opt-in per site rather than a blanket
    // swap — a site only consults this where it has an octant rendering to offer.
    Octants => octants, "octants", &[], Bool, bool, Genome, false,
        "draw supported charts with octant blocks instead of braille dots (needs a Unicode 16 font)";
    // Box drawing is BMP and near-universal in terminal fonts, so unlike `octants` this defaults
    // on and the ASCII spelling is the fallback rather than the other way round.
    Dividerbox => dividerbox, "dividerbox", &[], Bool, bool, Genome, true,
        "draw track dividers with box-drawing rules and rounded corners instead of ASCII arrows";
}

impl Setting {
    /// Resolves a setting name or one of its aliases.
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        static BY_NAME: OnceLock<HashMap<&'static str, Setting>> = OnceLock::new();
        BY_NAME
            .get_or_init(|| {
                Self::specs()
                    .iter()
                    .flat_map(|spec| {
                        std::iter::once((spec.name, spec.key))
                            .chain(spec.aliases.iter().map(|alias| (*alias, spec.key)))
                    })
                    .collect()
            })
            .get(name)
            .copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_read_color_name_round_trips_to_its_own_variant() {
        for (name, mode) in ReadColor::ALL {
            assert_eq!(ReadColor::parse(name), Some(mode));
            assert_eq!(mode.name(), name);
            assert_eq!(ReadColor::parse(&name.to_ascii_uppercase()), Some(mode));
        }
    }

    /// The parser is total over arbitrary input: every string is either a declared mode or `None`,
    /// never a panic and never a mode it does not name.
    #[test]
    fn parsing_arbitrary_text_as_a_read_color_never_panics_or_invents_a_mode() {
        let alphabet: Vec<char> = "direction default DIRECTIONdefault \0\u{1f600}-_0".chars().collect();
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        for _ in 0..4_000 {
            let len = (next() % 16) as usize;
            let candidate: String = (0..len).map(|_| alphabet[(next() as usize) % alphabet.len()]).collect();
            match ReadColor::parse(&candidate) {
                Some(mode) => assert!(candidate.eq_ignore_ascii_case(mode.name())),
                None => assert!(!ReadColor::ALL.iter().any(|(n, _)| candidate.eq_ignore_ascii_case(n))),
            }
        }
    }

    /// Every bounded default satisfies the bounds its own setting declares.
    ///
    /// Deliberately says nothing about *which* value each default holds — those exist to be
    /// retuned. A default outside its validator's bounds cannot be restored through `:set` once
    /// changed; this table-driven check proves the const assertion is wired to the declared value.
    #[test]
    fn every_bounded_default_satisfies_the_bounds_its_setting_declares() {
        let mut checked = 0;
        for spec in Setting::specs() {
            let SettingKind::BoundedInteger { min, max } = spec.kind else {
                continue;
            };
            let value = (spec.default)()
                .as_integer()
                .expect("a BoundedInteger setting holds an integer");
            assert!(
                (min..=max).contains(&value),
                "{}'s default {value} is outside its declared {min}..={max}",
                spec.name
            );
            checked += 1;
        }
        assert!(
            checked > 0,
            "no bounded setting was checked, so this test proves nothing"
        );
    }
}
