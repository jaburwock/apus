//! Application shell coordinating independent viewers, global input, drawing, and dispatch.
//!
//! `App` owns only shell state: the active viewer, keymap, command bar, palette pane, notification stack,
//! and shared I/O-worker health. Viewer state stays inside boxed `ViewerApp` instances.
//!
//! The active scopes resolve keys to typed commands or command-line text. The router delivers
//! the commands and any follow-ups.
//!
//! Drawing lays out a real palette pane before the viewer and status row, preserving invariant 9;
//! it then draws only the active viewer while allowing the bar and notifications to overlay it.
//! Search updates likewise target the active viewer only. Every viewer ticks, so a background tab's
//! load still finishes and reports, but only the active viewer's activity asks for a redraw.
//! Command routing enters
//! through `DispatchTarget`: this module performs shell, active-viewer, and broadcast effects
//! without exposing viewer state as a query.

mod tab_bar;

use ratatui::{Frame, layout::Rect, style::Style};

use self::tab_bar::TabBar;

use crate::Instant;
use crate::command::bar::{self, BarState, DrawView, HelpPage, HelpState};
use crate::command::protocol::{
    Applied, CommandOutcome, DispatchReport, DispatchTarget, HelpContent, LineEdit, ListMove, PaneCommand, ShellCommand,
};
use crate::command::router;
use crate::command::{self, CommandLineMode, HelpSection};
use crate::input::{Key, Modifiers, MouseEvent, MouseKind};
use crate::jobs::WorkerHealth;
use crate::keys::{Chord, Keymap, Policy, Resolution, Scope};
use crate::notification::{Diagnostic, DiagnosticLog, Notification, NotificationStack, Severity};
use crate::spinner::LoopState;
use crate::style::Surface;
use crate::style::palette;
use crate::style::pane::{PANE_WIDTH, PalettePane, PaneAction};
use crate::viewer::ViewerApp;

/// Shell over independent viewers, with app-global bar state and worker health.
pub struct App {
    viewers: Vec<Box<dyn ViewerApp>>,
    active: usize,
    tab_bar: TabBar,
    /// Viewer exposed to mouse input in the last rendered frame.
    mouse_viewer: Option<usize>,
    keymap: Keymap,
    quit: bool,
    pub(crate) bar: BarState,
    io_worker_health: Option<WorkerHealth>,
    /// Append-only record of every shell and drained viewer diagnostic emitted this session.
    diagnostics: DiagnosticLog,
    /// Timed visual projection over `diagnostics`, displayed until records expire.
    pub(crate) notifications: NotificationStack,
    /// Records already logged from each viewer (indexed like `viewers`), held for display until
    /// that viewer is active.
    unseen: Vec<Vec<Notification>>,
    /// Whether the worker's death has already been reported, so it is said once, not per frame.
    worker_death_reported: bool,
    /// The palette editor, present only while open. App-level: it edits app-global state and no
    /// viewer knows it exists.
    palette_pane: Option<PalettePane>,
}

impl App {
    /// `io_worker_health` drives the app-global worker warning.
    pub fn new(viewers: Vec<Box<dyn ViewerApp>>, io_worker_health: WorkerHealth) -> Self {
        Self::with_worker_health(viewers, Some(io_worker_health))
    }

    pub(crate) fn with_worker_health(viewers: Vec<Box<dyn ViewerApp>>, io_worker_health: Option<WorkerHealth>) -> Self {
        assert!(!viewers.is_empty(), "App needs at least one ViewerApp");
        Self {
            unseen: viewers.iter().map(|_| Vec::new()).collect(),
            viewers,
            active: 0,
            tab_bar: TabBar::default(),
            mouse_viewer: None,
            keymap: Keymap::defaults(),
            quit: false,
            bar: BarState::default(),
            io_worker_health,
            diagnostics: DiagnosticLog::default(),
            notifications: NotificationStack::default(),
            worker_death_reported: false,
            palette_pane: None,
        }
    }

    /// Runs a startup command batch. Startup chooses its non-failing policy and reports the
    /// returned failures once the shell is ready to draw them.
    pub fn run_command(&mut self, input: &str) -> DispatchReport {
        self.cancel_mouse_gesture();
        router::route_command(self, input, false)
    }

    /// Adds dispatch failures to the diagnostic log and notification stack with their origin's severity.
    pub fn report_dispatch(&mut self, report: DispatchReport, severity: Severity) {
        for failure in report.failures {
            self.record(Notification::new(severity, failure));
        }
    }

    /// All diagnostics recorded during this session, oldest first.
    pub fn diagnostics(&self) -> &[Diagnostic] {
        self.diagnostics.entries()
    }

    /// Whether any [`Severity::Error`] diagnostic has been recorded.
    pub fn has_errors(&self) -> bool {
        self.diagnostics.has_errors()
    }

    /// Whether any viewer is still waiting on background I/O, search convergence, or animation.
    ///
    /// Every viewer, not just the active one: a caller that waits for this to clear before reading
    /// [`Self::has_errors`] must not finish while a background tab's load can still fail.
    pub fn has_pending_work(&self) -> bool {
        self.viewers.iter().any(|viewer| viewer.has_pending_work())
    }

    /// Appends a record to the diagnostic log and projects it into the live notification stack.
    fn record(&mut self, notification: Notification) {
        self.diagnostics.push(notification.severity, notification.text.clone());
        self.notifications.push(notification);
    }

    /// Logs every viewer's pending records, then displays the active viewer's, returning whether
    /// any were displayed.
    ///
    /// Logging is immediate, so [`Self::has_errors`] sees a background tab's failure. Display waits
    /// until the tab is looked at, so a failure is seen in the context that produced it.
    fn drain_notifications(&mut self) -> bool {
        for (viewer, unseen) in self.viewers.iter_mut().zip(&mut self.unseen) {
            for notification in viewer.drain_notifications() {
                self.diagnostics.push(notification.severity, notification.text.clone());
                unseen.push(notification);
            }
        }
        let shown = std::mem::take(&mut self.unseen[self.active]);
        let had_any = !shown.is_empty();
        for notification in shown {
            self.notifications.push(notification);
        }
        had_any
    }

    /// Resolves a tab name against the viewers' own titles, so focus never depends on tab order and
    /// an absent tab is an unresolved name rather than a bad index.
    fn find_viewer(&self, name: &str) -> Option<usize> {
        self.viewers.iter().position(|v| v.title().eq_ignore_ascii_case(name))
    }

    /// The viewer Tab moves to.
    fn next_in_rotation(&self) -> usize {
        (self.active + 1) % self.viewers.len()
    }

    pub fn should_quit(&self) -> bool {
        self.quit
    }

    /// The focused tab's title, for tests that assert where focus landed by name rather than by
    /// index. Not a production read path: nothing in the app needs to ask.
    #[cfg(test)]
    pub(crate) fn active_title(&self) -> &'static str {
        self.viewers[self.active].title()
    }

    fn active_viewer(&mut self) -> &mut dyn ViewerApp {
        self.viewers[self.active].as_mut()
    }

    /// Splits content into the viewer area and the app-owned status row.
    fn split_status_row(content: Rect) -> (Rect, Rect) {
        let status_height = content.height.min(1);
        let viewer_area = Rect {
            height: content.height - status_height,
            ..content
        };
        let status_row = Rect {
            y: content.y + viewer_area.height,
            height: status_height,
            ..content
        };
        (viewer_area, status_row)
    }

    /// Splits content into the palette pane and the narrowed viewer area.
    ///
    /// The horizontal-axis counterpart of [`Self::split_status_row`]: the pane is a real pane
    /// beside the viewers, not an inset carved out of a track (invariant 9). Closed, or too narrow
    /// to leave a usable viewer column, it takes nothing.
    fn split_palette_pane(&self, content: Rect) -> (Option<Rect>, Rect) {
        // Below twice the pane's width the viewers would be squeezed into a column narrower than
        // the editor beside them, which is the wrong trade for a tool whose subject is the viewer.
        if self.palette_pane.is_none() || content.width < PANE_WIDTH * 2 {
            return (None, content);
        }
        let pane = Rect {
            width: PANE_WIDTH,
            ..content
        };
        let viewer_area = Rect {
            x: content.x + PANE_WIDTH,
            width: content.width - PANE_WIDTH,
            ..content
        };
        (Some(pane), viewer_area)
    }

    /// Draws the active viewer and the app-owned bar.
    pub fn draw(&mut self, frame: &mut Frame, tab_bar: Rect, content: Rect, loop_state: LoopState) {
        // The app owns its ground: with `Color::Reset` gone nothing else would paint the cells no
        // widget covers, and an unpainted cell is the one place a contrast claim cannot reach.
        let frame_area = frame.area();
        frame
            .buffer_mut()
            .set_style(frame_area, Style::default().bg(Surface::APP_BG()));

        self.tab_bar.draw(frame, tab_bar, &self.viewers, self.active);

        self.drain_notifications();
        // The "still displaying something" answer is deliberately dropped rather than fed back as
        // redraw activity: keeping the loop awake for a record's whole lifetime would burn frames
        // for ten seconds to expire a warning nobody is watching. An idle record therefore outstays
        // its lifetime until the next input, and the first key or mouse movement clears it.
        self.notifications.tick();

        self.note_worker_death();

        // The pane takes its column first, so the viewer and every bar overlay are laid out
        // against the narrowed area rather than drawn and then covered.
        let (pane_area, content) = self.split_palette_pane(content);
        let (viewer_area, status_row) = Self::split_status_row(content);
        self.mouse_viewer = self.mouse_target();
        let help = self.bar.help.as_ref().map(|help| HelpPage {
            sections: self.help_sections(),
            hint: self.help_hint(help),
        });

        let Self {
            viewers,
            active,
            bar,
            notifications,
            palette_pane,
            keymap,
            ..
        } = self;
        let viewer = viewers[*active].as_mut();
        viewer.draw(frame, viewer_area, keymap);

        let resolved = viewer.search_matches();
        let formatted = viewer.format_matches(resolved);
        let records: Vec<_> = notifications.visible().collect();
        let view = DrawView::new(
            &*viewer,
            viewer_area,
            status_row,
            loop_state,
            &records,
            &formatted,
            help,
        );
        bar::draw(frame, bar, view);

        if let (Some(pane), Some(area)) = (palette_pane.as_mut(), pane_area) {
            pane.draw(frame, area, keymap);
        }
    }

    /// Reports the I/O worker's death once, when first observed.
    ///
    /// Emitted here rather than where the worker dies because nothing owns that moment — health
    /// is a flag the shell polls. Once is enough: the record is an `Error`, so it never expires.
    fn note_worker_death(&mut self) -> bool {
        if self.worker_death_reported || self.io_worker_health.as_ref().is_none_or(WorkerHealth::is_alive) {
            return false;
        }
        self.worker_death_reported = true;
        self.record(Notification::new(
            Severity::Error,
            "I/O worker stopped — loading and BAM fetching unavailable".to_string(),
        ));
        true
    }

    fn dispatch_interactive(&mut self, outcome: CommandOutcome) {
        if let Err(error) = router::route_outcome(self, outcome) {
            self.record(Notification::new(Severity::Error, error.message));
        }
    }

    fn submit_command(&mut self, input: &str) {
        match router::route_bar_command(self, input) {
            Ok(()) => {}
            Err(error) if error.follow_up => self.record(Notification::new(Severity::Error, error.message)),
            Err(error) => self.bar.flash_error(error.message),
        }
    }

    fn scopes(&self) -> Vec<Scope> {
        let mut stack = Vec::new();
        if let Some(help) = &self.bar.help {
            if help.draft.is_some() {
                stack.push(Scope::CommandLine);
            }
            stack.push(Scope::HelpOverlay);
        }
        stack.extend(self.scopes_under_help());
        stack
    }

    /// The stack `:shortcuts` lists: everything help covers.
    fn scopes_under_help(&self) -> Vec<Scope> {
        let mut stack = Vec::new();
        if let Some(pane) = &self.palette_pane {
            stack.push(pane.scope());
        }
        if self.bar.command_line.is_some() {
            stack.push(Scope::CommandLine);
        }
        stack.extend(self.viewers[self.active].scopes());
        stack.push(Scope::Global);
        stack
    }

    /// The open help popover's rows, rebuilt on each use so shortcuts follow the current view.
    fn help_sections(&self) -> Vec<HelpSection> {
        match self.bar.help.as_ref().map(|help| help.content) {
            None => Vec::new(),
            Some(HelpContent::Settings) => {
                let values: Vec<_> = self.viewers.iter().flat_map(|viewer| viewer.setting_values()).collect();
                command::help_sections(&values)
            }
            Some(HelpContent::Shortcuts) => self.keymap.help_sections(&self.scopes_under_help()),
        }
    }

    fn help_hint(&self, help: &HelpState) -> String {
        if help.draft.is_some() {
            return [(LineEdit::Submit, "apply"), (LineEdit::Cancel, "cancel")]
                .into_iter()
                .filter_map(|(edit, caption)| {
                    let key = self
                        .keymap
                        .primary_key(Scope::CommandLine, &ShellCommand::Line(edit).into())?;
                    Some(format!("{key} {caption}"))
                })
                .collect::<Vec<_>>()
                .join(" ");
        }
        let select = (help.content == HelpContent::Settings).then_some(ListMove::Select);
        let moves = select.into_iter().chain([ListMove::Close]);
        self.keymap.hints(
            Scope::HelpOverlay,
            moves.map(|action| ShellCommand::Help(action).into()),
            "  ",
        )
    }

    /// Returns whether the application handled or swallowed the key.
    pub fn on_key(&mut self, key: Key, modifiers: Modifiers) -> bool {
        self.cancel_mouse_gesture();
        match self.keymap.resolve(Chord::new(key, modifiers), &self.scopes()) {
            Resolution::Bound(outcome) => {
                let blocked = outcome.descriptor().is_some_and(|entry| entry.blocked_while_busy)
                    && self.viewers[self.active].background_work().is_some();
                if !blocked {
                    self.dispatch_interactive(outcome.clone());
                }
            }
            Resolution::Text(character) => self.bar.type_character(character),
            Resolution::Swallowed => {}
            Resolution::Unbound => return false,
        }
        true
    }

    fn edit_line(&mut self, edit: LineEdit) {
        if let Some(help) = &mut self.bar.help
            && let Some(draft) = &mut help.draft
        {
            match edit {
                LineEdit::Cancel => help.draft = None,
                LineEdit::Backspace => draft.backspace(),
                LineEdit::Submit => {
                    let resolved = command::parse::resolve_value(draft.spec.key, draft.spec.name, &draft.input);
                    let result = resolved.and_then(|value| {
                        router::route_outcome(self, CommandOutcome::Setting(value)).map_err(|error| error.message)
                    });
                    if let Some(help) = &mut self.bar.help {
                        match result {
                            Ok(()) => help.draft = None,
                            Err(error) => {
                                if let Some(draft) = &mut help.draft {
                                    draft.error = Some(error);
                                }
                            }
                        }
                    }
                }
            }
            return;
        }
        match edit {
            LineEdit::Cancel => self.bar.command_line = None,
            LineEdit::Backspace => {
                if let Some(line) = &mut self.bar.command_line {
                    line.backspace();
                }
            }
            LineEdit::Submit => {
                let Some(line) = self.bar.command_line.take() else {
                    return;
                };
                if line.mode == CommandLineMode::Search {
                    self.submit_search();
                } else {
                    self.submit_command(&line.input);
                }
            }
        }
    }

    fn submit_search(&mut self) {
        let viewer = self.active_viewer();
        if let Some(selected) = viewer.search_matches().first().cloned()
            && let Some(text) = viewer.search_enter(&selected)
        {
            self.bar.flash(text);
        }
    }

    fn apply_palette(&mut self, command: PaneCommand) -> Applied {
        let Some(pane) = &mut self.palette_pane else {
            return Applied::Ok;
        };
        match pane.apply(command) {
            PaneAction::None => {}
            PaneAction::Close => self.palette_pane = None,
            PaneAction::Save => {
                if self.io_worker_health.is_none() {
                    return Applied::Err("Saving a palette file is unavailable in this demo".to_string());
                }
                let path = std::path::Path::new(palette::file::ACTIVE_PALETTE);
                let message = match palette::file::save(path) {
                    Ok(()) => format!("palette written to {}", path.display()),
                    Err(message) => return Applied::Err(message),
                };
                self.record(Notification::new(Severity::Info, message));
            }
            PaneAction::SelectPalette(name) => {
                return Applied::FollowUps(vec![ShellCommand::SetColorscheme(name.to_string()).into()]);
            }
        }
        Applied::Ok
    }

    fn mouse_target(&self) -> Option<usize> {
        (self.bar.help.is_none() && self.bar.command_line.is_none()).then_some(self.active)
    }

    /// Routes clicks to shell controls and forwards gestures to the exposed viewer.
    pub fn on_mouse(&mut self, mouse: MouseEvent) {
        let cell = mouse.position();
        if let Some(help) = &mut self.bar.help {
            match mouse.kind {
                MouseKind::Moved => help.hover(cell),
                MouseKind::LeftDown if !help.contains(cell) => {
                    self.dispatch_interactive(ShellCommand::Help(ListMove::Close).into());
                }
                MouseKind::LeftDown => {
                    let sections = self.help_sections();
                    if let Some(value) = self.bar.click_help(cell, &sections) {
                        self.dispatch_interactive(CommandOutcome::Setting(value));
                    }
                }
                MouseKind::LeftDrag | MouseKind::LeftUp => {}
            }
            return;
        }
        let Some(index) = self.mouse_target().filter(|&index| self.mouse_viewer == Some(index)) else {
            return;
        };
        // A viewer popover sees outside presses even when they land over the toolbar.
        if mouse.kind == MouseKind::LeftDown
            && self.tab_bar.contains(cell)
            && self.viewers[index]
                .scopes()
                .first()
                .is_none_or(|scope| scope.policy() != Policy::Capture)
        {
            self.cancel_mouse_gesture();
            if self.scopes().iter().all(|scope| scope.policy() == Policy::Transparent)
                && let Some(command) = self.tab_bar.command_at(cell)
            {
                self.dispatch_interactive(command);
            }
        } else if let Some(outcome) = self.viewers[index].on_mouse(mouse) {
            self.dispatch_interactive(outcome);
        }
    }

    /// Ends held mouse navigation when the frontend loses focus or changes the grid.
    pub fn cancel_mouse_gesture(&mut self) {
        self.active_viewer().cancel_mouse_gesture();
    }

    fn focus_viewer(&mut self, index: usize) {
        if index != self.active {
            self.cancel_mouse_gesture();
            self.mouse_viewer = None;
            self.active = index;
        }
    }

    /// Wakes an idle frontend for the next held mouse step.
    pub fn mouse_repeat_deadline(&self) -> Option<Instant> {
        self.viewers[self.active].mouse_repeat_deadline()
    }

    /// Ticks every viewer and the bar, drains notifications, and checks worker health, returning
    /// whether the active view needs a redraw.
    pub fn tick(&mut self) -> bool {
        let query = self
            .bar
            .command_line
            .as_ref()
            .and_then(|cl| (cl.mode == CommandLineMode::Search).then(|| cl.input[1..].to_string()));
        if let Some(query) = query {
            self.active_viewer().search_tick(&query);
        }

        let mut animating = false;
        for (index, viewer) in self.viewers.iter_mut().enumerate() {
            let active = viewer.tick();
            animating |= active && index == self.active;
        }
        let drained = self.drain_notifications();
        let worker_died = self.note_worker_death();
        animating | drained | worker_died | self.bar.tick()
    }
}

/// The shell half of dispatch. The router classifies audience; these perform the operation with
/// state only `App` owns, which is why the router never needs the viewer list.
impl DispatchTarget for App {
    fn apply_shell(&mut self, command: ShellCommand) -> Applied {
        match command {
            ShellCommand::OpenHelp(content) => {
                self.cancel_mouse_gesture();
                self.mouse_viewer = None;
                self.bar.help = Some(HelpState::new(content));
                Applied::Ok
            }
            ShellCommand::Help(list_move) => {
                let sections = self.help_sections();
                match self.bar.move_help(list_move, &sections) {
                    Some(value) => Applied::FollowUps(vec![CommandOutcome::Setting(value)]),
                    None => Applied::Ok,
                }
            }
            ShellCommand::OpenLine(trigger) => {
                self.cancel_mouse_gesture();
                self.mouse_viewer = None;
                self.bar.open_command_line(trigger.character());
                Applied::Ok
            }
            ShellCommand::Line(edit) => {
                self.edit_line(edit);
                Applied::Ok
            }
            ShellCommand::NextTab => {
                self.focus_viewer(self.next_in_rotation());
                Applied::Ok
            }
            ShellCommand::Quit => {
                self.quit = true;
                Applied::Ok
            }
            ShellCommand::Palette(command) => self.apply_palette(command),
            ShellCommand::FocusTab(name) => match self.find_viewer(&name) {
                Some(index) => {
                    self.focus_viewer(index);
                    Applied::Ok
                }
                None => Applied::Err(format!("No tab named {name}")),
            },
            ShellCommand::OpenPalettePane => {
                self.palette_pane = Some(PalettePane::default());
                Applied::Ok
            }
            // Unlike most `:set`s this validates on apply, because only `style` knows the ramp
            // vocabulary.
            ShellCommand::SetPalette(spec) => match crate::style::palette::apply_spec(&spec) {
                Ok(_) => Applied::Ok,
                Err(message) => Applied::Err(message),
            },
            // Validated on apply for the same reason: only `style` knows which palettes exist.
            ShellCommand::SetColorscheme(name) => match palette::builtin::apply_named(&name) {
                Ok(_) => Applied::Ok,
                Err(message) => Applied::Err(message),
            },
        }
    }

    fn apply_active(&mut self, outcome: CommandOutcome) -> Applied {
        self.viewers[self.active].apply_outcome(outcome)
    }

    fn apply_broadcast(&mut self, outcome: CommandOutcome) -> Applied {
        let mut failure = None;
        let mut follow_ups = Vec::new();
        // Unrelated viewers decline a broadcast; an all-decline result means no recipient claimed it.
        let mut handled = false;
        for viewer in self.viewers.iter_mut() {
            match viewer.apply_outcome(outcome.clone()) {
                Applied::Ok => handled = true,
                Applied::FollowUps(outcomes) => {
                    handled = true;
                    follow_ups.extend(outcomes);
                }
                Applied::Err(message) if failure.is_none() => failure = Some(message),
                Applied::NotApplicable | Applied::Err(_) => {}
            }
        }
        match failure {
            Some(message) => Applied::Err(message),
            None if !follow_ups.is_empty() => Applied::FollowUps(follow_ups),
            None if handled => Applied::Ok,
            None => Applied::NotApplicable,
        }
    }
}

#[cfg(test)]
mod tests;
