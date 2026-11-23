//! Resolved command messages and dispatch contracts.
//!
//! This is the command/`viewer_app` seam. [`CommandOutcome`] and
//! [`FeatureFilterEdit`] are plain resolved messages; they carry enough
//! information for a receiver to act without exposing viewer state to command
//! resolution.
//!
//! [`CommandTarget`] is the viewer half: a concrete viewer applies an outcome
//! using only state it owns. Its default [`Applied::NotApplicable`] response
//! permits broadcast dispatch without capability queries.
//!
//! [`DispatchTarget`] is the shell half: the router may request shell, active-
//! viewer, or broadcast application. It is a call path, not a read path:
//! nothing returns viewer state.
//!
//! Every outcome here is scalar: a batch is a `Vec` of
//! [`crate::command::parse::ResolvedLine`] the router walks, never an outcome of
//! its own. [`Applied`] also lets a receiver emit typed follow-ups for the
//! router to deliver after the current outcome.

use std::path::PathBuf;

use crate::basepair::BasePair;
use crate::setting::SettingValue;
use crate::vcf_file::AlternateSampleLookup;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeatureFilterEdit {
    Include(Vec<String>),
    Exclude(Vec<String>),
    Clear,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollStep {
    Near,
    NearX10,
    Mid,
    Far,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListMove {
    Up,
    Down,
    Select,
    Close,
}

/// What the help popover lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpContent {
    /// `:settings` and `:help`, the editable settings and command reference.
    Settings,
    /// `:shortcuts`, the keys reachable from the current view.
    Shortcuts,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneCommand {
    List(ListMove),
    Close,
    Save,
    ToggleReach,
    Move(Direction),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEdit {
    Cancel,
    Submit,
    Backspace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineTrigger {
    Command,
    Slash,
    Contig,
}

impl LineTrigger {
    pub fn character(self) -> char {
        match self {
            Self::Command => ':',
            Self::Slash => '/',
            Self::Contig => 'c',
        }
    }
}

/// A resolved command, ready for a receiver to apply.
///
/// Grouped by recipient. [`Self::Shell`], [`Self::Genome`] and [`Self::Table`] each have exactly
/// one owner, which matches its inner enum exhaustively; every other receiver ignores the whole
/// group in one arm. The remaining variants are shared: each viewer reads them on its own axis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandOutcome {
    None,
    /// Applied by `App`; no viewer sees these.
    Shell(ShellCommand),
    /// Applied by the genome viewer.
    Genome(GenomeCommand),
    /// Applied by the variant table.
    Table(TableCommand),
    /// A goto target as typed. Always lands centred in the viewport.
    ///
    /// Carries two readings of the same text because resolution is app-global:
    /// `crate::command::parse::resolve_command` takes only a string, so it
    /// cannot know which viewer will receive this. Each recipient reads the
    /// field its own axis is denominated in and ignores the other.
    Goto {
        horizontal: Option<BasePair>,
        vertical: Option<BasePair>,
        /// The same text read as a row number: `Some` only for a bare integer.
        /// `None` for suffixed or two-axis input, which is genome grammar — a
        /// table row is a grid index, not bp (invariant 2), so the VCF viewer
        /// rejects those rather than guessing at a conversion.
        line: Option<u64>,
    },
    Scroll {
        direction: Direction,
        step: ScrollStep,
    },
    /// Moves whichever list overlay the active viewer has open.
    List(ListMove),
    JumpToPeer,
    /// A parsed setting value broadcast to listener viewers; writes and constraints remain owned.
    Setting(SettingValue),
}

/// Outcomes that change app-owned state: focus, help, the command line, the palette.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellCommand {
    OpenHelp(HelpContent),
    /// Moves or closes the help popover. Its own variant because a plain `List` goes to the
    /// active viewer.
    Help(ListMove),
    /// A viewer title to focus.
    FocusTab(String),
    OpenPalettePane,
    /// A palette edit, as the raw `<entry>:<colour>` spec.
    ///
    /// Carried unparsed because ramp names and step bounds are facts about the palette, so `style`
    /// is the only module that reads this.
    SetPalette(String),
    /// A built-in palette to select whole, by name. Unparsed for the same reason as
    /// [`Self::SetPalette`].
    SetColorscheme(String),
    Palette(PaneCommand),
    Line(LineEdit),
    OpenLine(LineTrigger),
    NextTab,
    Quit,
}

/// Outcomes only the genome viewer applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GenomeCommand {
    /// A contig name as typed, resolved against the viewer's own index on apply.
    ActiveContig(String),
    OpenContigSelector,
    OpenTrackPicker,
    CycleReadColor,
    OpenBamSwitcher,
    /// A typed VCF follow-up. Genome decides whether it has BAMs to choose between.
    ChooseVariantBam {
        contig: String,
        position: BasePair,
        samples: AlternateSampleLookup,
    },
    JumpToVariant {
        contig: String,
        position: BasePair,
        bam: Option<PathBuf>,
    },
    /// One axis per command; applying either requires viewer-owned recentering.
    Zoom {
        horizontal_to_mid: Option<bool>,
        alignment_v_to_mid: Option<bool>,
    },
    StepZoom {
        into: bool,
    },
    /// Resolved paths only. The load itself happens after routing.
    Load {
        gtf_path: PathBuf,
        cache_path: PathBuf,
        /// `:load_index` builds a missing cache; `:load` fails instead, naming the
        /// `:load_index` line that would build one.
        ///
        /// A field rather than a second variant: the two commands differ in this one decision and
        /// share a payload. It travels all the way to the worker, which is where the cache is
        /// checked — see [`crate::locus::load_only`].
        build_missing: bool,
    },
    /// The lookup happens on apply because it needs viewer-owned feature state.
    JumpToFeature {
        forward: bool,
    },
    FeatureFilter(FeatureFilterEdit),
    /// Swap the reference genome; opening and validation happen on apply.
    ActiveFasta(PathBuf),
    /// Remember the path and open it as the active BAM.
    ActiveBam(PathBuf),
    /// Remember a BAM path for later selection.
    AddBam(PathBuf),
}

/// Outcomes only the variant table applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableCommand {
    /// Swap the variant table's VCF; opening and validation happen on apply. Broadcast, so it
    /// reaches the table from any tab.
    ActiveVcf(PathBuf),
    /// Finds the row nearest to a 0-based locus coordinate on the named contig.
    /// The lookup is viewer-owned and asynchronous because it needs the VCF's
    /// line index and file path.
    GotoLocus {
        contig: String,
        position: BasePair,
    },
    OpenVariantBams {
        contig: String,
        position: BasePair,
        samples: AlternateSampleLookup,
        paths: Vec<PathBuf>,
        active: Option<PathBuf>,
    },
    Page {
        up: bool,
    },
    ColumnPage {
        left: bool,
    },
    OpenColumnPicker,
    ToggleColumn,
    ShowAllColumns,
    ShowOnlyColumn,
}

impl From<ShellCommand> for CommandOutcome {
    fn from(command: ShellCommand) -> Self {
        Self::Shell(command)
    }
}

impl From<GenomeCommand> for CommandOutcome {
    fn from(command: GenomeCommand) -> Self {
        Self::Genome(command)
    }
}

impl From<TableCommand> for CommandOutcome {
    fn from(command: TableCommand) -> Self {
        Self::Table(command)
    }
}

/// What a receiver did with a dispatched command outcome.
///
/// `NotApplicable` is not a failure: it is the expected answer from a viewer
/// that does not own a broadcast outcome. Targeted dispatch converts it to a
/// user-facing error, while broadcast dispatch ignores it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    Ok,
    FollowUps(Vec<CommandOutcome>),
    NotApplicable,
    Err(String),
}

/// The failures collected while resolving and applying one command batch.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DispatchReport {
    pub failures: Vec<String>,
}

impl DispatchReport {
    pub fn is_ok(&self) -> bool {
        self.failures.is_empty()
    }

    /// A report of one failure that happened before any command ran — a palette file that could
    /// not be read, say. Public so startup can report such a failure through the same path as a
    /// dispatch failure rather than accumulating it for a later batch.
    pub fn failure(message: String) -> Self {
        Self {
            failures: vec![message],
        }
    }
}

impl Applied {
    /// Focus `tab`, then apply `steps` there. Focus must come first: non-broadcast steps reach
    /// whichever viewer is active when they are routed.
    pub fn in_tab(tab: &str, steps: impl IntoIterator<Item = CommandOutcome>) -> Self {
        let focus = CommandOutcome::Shell(ShellCommand::FocusTab(tab.to_string()));
        Self::FollowUps(std::iter::once(focus).chain(steps).collect())
    }

    pub(crate) fn into_follow_ups(self) -> Result<Vec<CommandOutcome>, String> {
        match self {
            Self::Ok => Ok(Vec::new()),
            Self::FollowUps(outcomes) => Ok(outcomes),
            Self::NotApplicable => Err("command not applicable here".to_string()),
            Self::Err(message) => Err(message),
        }
    }

    #[cfg(test)]
    pub(crate) fn into_target_result(self) -> Result<(), String> {
        self.into_follow_ups().map(|_| ())
    }
}

/// The shell's half of dispatch: what the router may ask the app to do with a resolved outcome.
///
/// The counterpart of [`CommandTarget`], which is how a *viewer* applies one to itself. The router
/// classifies an outcome's audience and calls the matching method; the implementor performs the
/// operation using state only it owns. This keeps the router free of the viewer list — it never sees
/// `viewers`, so there is no second borrow to conflict with `&mut App`.
///
/// It is a call path from `command` into the shell, but not a read path: nothing here
/// returns viewer state, and the three methods name exactly what a command may reach.
pub trait DispatchTarget {
    /// Applies an outcome the shell owns — focus, help, the palette. No viewer sees these.
    fn apply_shell(&mut self, command: ShellCommand) -> Applied;

    /// Applies an outcome to the active viewer, read at call time so a preceding
    /// focus change redirects it.
    fn apply_active(&mut self, outcome: CommandOutcome) -> Applied;

    /// Applies an outcome to every viewer, for outcomes whose recipient is not the active tab.
    fn apply_broadcast(&mut self, outcome: CommandOutcome) -> Applied;
}

/// The command dispatch seam. Text resolution belongs to `crate::command`; viewers apply already-
/// resolved outcomes to their own state.
pub trait CommandTarget {
    fn apply_outcome(&mut self, _outcome: CommandOutcome) -> Applied {
        Applied::NotApplicable
    }
}
