//! Contracts for top-level viewers.
//!
//! Viewers report their active key scopes and apply resolved commands. They also draw, tick,
//! handle supplemental mouse input, and optionally provide search results to the shell.
//!
//! `StatusProvider` is the shell's one-way status seam. Viewers contribute optional status text,
//! drainable notifications, and a description of in-flight work; the shell owns presentation,
//! expiry, and layout. Mouse input is supplemental, and default methods keep optional capabilities
//! from requiring shell-side special cases.

use crate::Instant;

use ratatui::{Frame, layout::Rect};

use crate::command::protocol::{CommandOutcome, CommandTarget};
use crate::input::MouseEvent;
use crate::keys::{Keymap, Scope};
use crate::notification::Notification;
use crate::search::ResolvedMatch;
use crate::setting::SettingValue;

/// A top-level independently-tabbed application.
pub trait ViewerApp: CommandTarget + StatusProvider {
    /// This viewer's identity: the name `:focustab` resolves.
    fn title(&self) -> &'static str;

    /// Human-facing name shown in the tab bar and the status row.
    ///
    /// Separate from [`Self::title`] so a shorter display name never silently changes the spelling
    /// a command has to use. Defaults to the identity for viewers whose two names agree.
    fn display_name(&self) -> &'static str {
        self.title()
    }

    /// Draws this viewer's content into `area` with the shell's bindings available for hints.
    fn draw(&mut self, frame: &mut Frame, area: Rect, keymap: &Keymap);

    /// Active key scopes, ordered from the innermost overlay to the viewer itself.
    fn scopes(&self) -> Vec<Scope> {
        Vec::new()
    }

    /// The current value of every setting this viewer owns, for `:help` to display.
    fn setting_values(&self) -> Vec<SettingValue> {
        Vec::new()
    }

    /// Handles supplemental mouse input.
    fn on_mouse(&mut self, _mouse: MouseEvent) -> Option<CommandOutcome> {
        None
    }

    /// Ends any held pointer gesture when input focus or layout changes.
    fn cancel_mouse_gesture(&mut self) {}

    /// When the active viewer next needs a tick for held mouse input.
    fn mouse_repeat_deadline(&self) -> Option<Instant> {
        None
    }

    /// Advances in-flight work and reports whether to redraw.
    fn tick(&mut self) -> bool;

    /// Whether this viewer is still waiting on background I/O, search convergence, or animation.
    ///
    /// Separate from [`StatusProvider::background_work`], which blocks `.busy()` keys and covers
    /// only one-off loads. Callers waiting for the view to settle poll [`Self::tick`] until this
    /// returns `false`. No default: a viewer that silently reported "settled" would let a headless
    /// capture finish early.
    fn has_pending_work(&self) -> bool;

    /// Advances this viewer's search session.
    fn search_tick(&mut self, _query: &str) {}

    /// Returns the resolved matches currently displayed.
    fn search_matches(&self) -> &[ResolvedMatch] {
        &[]
    }

    /// Formats resolved search matches into display rows.
    fn format_matches(&self, resolved: &[ResolvedMatch]) -> Vec<MatchRow> {
        let _ = resolved;
        Vec::new()
    }

    /// Applies the selected search match.
    fn search_enter(&mut self, _selected: &ResolvedMatch) -> Option<String> {
        None
    }
}

/// One display-ready row in the `/` search overlay.
pub struct MatchRow {
    pub text: String,
    pub bold_char_indices: Vec<u32>,
}

/// A viewer's contributions to the app-global status row.
pub trait StatusProvider {
    fn status_override(&self) -> Option<String> {
        None
    }

    /// Takes the records this viewer has emitted since the last frame.
    ///
    /// Many per frame, not one: a viewer can finish several jobs in a tick, and every record must
    /// survive. Severity crosses this seam with the text, so a failure still reads as a failure
    /// once the shell displays it.
    fn drain_notifications(&mut self) -> Vec<Notification> {
        Vec::new()
    }

    /// Reporting work blocks every `.busy()` key (tab switch, peer jump, contig selector) until it
    /// ends, so report only one-off loads. Viewport fetches (read windows, reference chunks, the
    /// local-area summary) redraw through `tick` and never report here.
    fn background_work(&self) -> Option<BackgroundWork> {
        None
    }
}

/// In-flight work one viewer wants surfaced by the shell.
pub struct BackgroundWork {
    /// Owned so a label can name the file or count it is working on.
    pub tasks: Vec<String>,
    pub started_at: Instant,
}
