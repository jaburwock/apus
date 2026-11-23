//! The app-global status bar, command-line input, and transient overlays.
//!
//! `App` owns the shell and delegates bar input and rendering here; the bar does not
//! inspect viewer state or route commands itself. `DrawView` carries the immutable
//! per-frame inputs needed after the active viewer has drawn.
//!
//! The bar owns command-line state, flashes, the help popover's content choice and cursor, and
//! the cached RSS readout. Help's rows come from `App` each frame in [`HelpPage`], because the
//! shortcut rows are read from the keymap and the bar holds none. Its fixed status columns keep
//! command feedback from overwriting the memory and activity indicators, while overlays are drawn
//! over the completed frame.
//!

#[cfg(not(target_arch = "wasm32"))]
mod mem;
mod overlays;

use crate::Instant;

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Position, Rect},
    style::{Color, Style},
    text::Line as TextLine,
    widgets::{Clear, Paragraph},
};

use crate::spinner::LoopState;
use crate::spinner::{FlashMessage, draw_activity_glyphs, spinner_glyph};
#[cfg(not(target_arch = "wasm32"))]
use mem::MemoryReadout;
use overlays::{draw_help_overlay, draw_notifications, draw_search_popover, search_popover_area};

use crate::command::HelpSection;
use crate::command::line::{CommandLineMode, CommandLineState};
use crate::command::protocol::{HelpContent, LineTrigger, ListMove};
use crate::list_cursor::ListCursor;
use crate::notification::Notification;
use crate::popover::VisibleRows;
use crate::setting::{SettingKind, SettingSpec, SettingValue};
use crate::style::{Selection, Surface, Text};
use crate::viewer::{MatchRow, ViewerApp};

/// Immutable per-frame inputs for bar rendering.
pub(crate) struct DrawView<'a> {
    viewer: &'a dyn ViewerApp,
    viewer_area: Rect,
    status_row: Rect,
    loop_state: LoopState,
    /// The shell's live records, oldest first.
    notifications: &'a [&'a Notification],
    formatted_matches: &'a [MatchRow],
    /// `Some` exactly when help is open.
    help: Option<HelpPage>,
}

impl<'a> DrawView<'a> {
    pub(crate) fn new(
        viewer: &'a dyn ViewerApp,
        viewer_area: Rect,
        status_row: Rect,
        loop_state: LoopState,
        notifications: &'a [&'a Notification],
        formatted_matches: &'a [MatchRow],
        help: Option<HelpPage>,
    ) -> Self {
        Self {
            viewer,
            viewer_area,
            status_row,
            loop_state,
            notifications,
            formatted_matches,
            help,
        }
    }
}

/// The rows of the open help popover and its key hints.
pub(crate) struct HelpPage {
    pub(crate) sections: Vec<HelpSection>,
    pub(crate) hint: String,
}

pub(crate) struct HelpState {
    pub(crate) content: HelpContent,
    pub(crate) cursor: ListCursor,
    pub(crate) draft: Option<SettingDraft>,
    visible_rows: Option<VisibleRows>,
}

impl HelpState {
    pub(crate) fn new(content: HelpContent) -> Self {
        Self {
            content,
            cursor: match content {
                HelpContent::Settings => ListCursor::at(1),
                HelpContent::Shortcuts => ListCursor::default(),
            },
            draft: None,
            visible_rows: None,
        }
    }

    pub(crate) fn hover(&mut self, cell: Position) {
        if self.draft.is_none()
            && let Some(row) = self.visible_rows.and_then(|rows| rows.index_at(cell))
        {
            self.cursor.select(row);
        }
    }

    pub(crate) fn contains(&self, cell: Position) -> bool {
        self.visible_rows.is_some_and(|rows| rows.bounds().contains(cell))
    }
}

pub(crate) struct SettingDraft {
    pub(crate) spec: &'static SettingSpec,
    pub(crate) input: String,
    pub(crate) error: Option<String>,
    replace_on_type: bool,
}

impl SettingDraft {
    fn push(&mut self, character: char) {
        if std::mem::take(&mut self.replace_on_type) {
            self.input.clear();
        }
        self.input.push(character);
        self.error = None;
    }

    pub(crate) fn backspace(&mut self) {
        self.input.pop();
        self.replace_on_type = false;
        self.error = None;
    }
}

/// Global status, command-line, and overlay state.
#[derive(Default)]
pub struct BarState {
    /// Active command line, if any.
    pub command_line: Option<CommandLineState>,
    /// Timed application message.
    pub(crate) flash_message: Option<FlashMessage>,
    /// The open help popover.
    pub(crate) help: Option<HelpState>,
    /// Throttled process RSS readout.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) memory: MemoryReadout,
}

impl BarState {
    pub fn flash(&mut self, text: String) {
        self.flash_message = Some(FlashMessage::new(text));
    }

    /// A command that could not be run. Kept in the status row, where the user is typing, rather
    /// than sent to the corner: it is feedback on the keystroke just made.
    pub fn flash_error(&mut self, text: String) {
        self.flash_message = Some(FlashMessage::new(text));
    }

    pub(crate) fn type_character(&mut self, character: char) {
        if let Some(draft) = self.help.as_mut().and_then(|help| help.draft.as_mut()) {
            draft.push(character);
        } else if let Some(line) = &mut self.command_line {
            line.push(character);
        }
    }

    pub(crate) fn click_help(&mut self, cell: Position, sections: &[HelpSection]) -> Option<SettingValue> {
        let help = self.help.as_mut()?;
        if help.content != HelpContent::Settings {
            return None;
        }
        let row = help.visible_rows.and_then(|rows| rows.index_at(cell));
        if help.draft.is_some() && row == Some(help.cursor.selected()) {
            return None;
        }
        help.draft = None;
        let row = row?;
        let setting = overlays::help_entry_at(sections, row)?.setting.as_ref()?;
        setting.value.as_ref()?;
        match setting.spec.key.kind() {
            SettingKind::ReadColor => None,
            SettingKind::Bool | SettingKind::BasePair | SettingKind::BoundedInteger { .. } => {
                help.cursor.select(row);
                self.move_help(ListMove::Select, sections)
            }
        }
    }

    pub(crate) fn move_help(&mut self, list_move: ListMove, sections: &[HelpSection]) -> Option<SettingValue> {
        let help = self.help.as_mut()?;
        if help.draft.is_some() && list_move != ListMove::Close {
            return None;
        }
        match list_move {
            ListMove::Up => help.cursor.up(),
            ListMove::Down => help.cursor.down(overlays::help_row_count(sections)),
            ListMove::Close => self.help = None,
            ListMove::Select => {
                if help.content != HelpContent::Settings {
                    return None;
                }
                let entry = overlays::help_entry_at(sections, help.cursor.selected())?;
                if let Some(setting) = &entry.setting {
                    let value = setting.value.as_ref()?;
                    if let Some(enabled) = value.as_bool() {
                        return SettingValue::from_bool(setting.spec.key, !enabled);
                    }
                    help.draft = Some(SettingDraft {
                        spec: setting.spec,
                        input: value.command_arg(),
                        error: None,
                        replace_on_type: true,
                    });
                } else {
                    self.help = None;
                    self.open_command_line(LineTrigger::Command.character());
                    for character in entry.key.chars().chain([' ']) {
                        self.type_character(character);
                    }
                }
            }
        }
        None
    }

    /// Opens a command line and clears transient UI state.
    pub fn open_command_line(&mut self, trigger: char) {
        self.flash_message = None;
        self.command_line = Some(CommandLineState::new(trigger));
    }

    /// Expires transient messages and refreshes the memory readout.
    pub fn tick(&mut self) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        self.memory.refresh();
        match &self.flash_message {
            Some(flash) if flash.is_expired() => {
                self.flash_message = None;
                false
            }
            Some(_) => true,
            None => false,
        }
    }
}

/// Formats command-line text for display.
fn command_line_display_text(command_line: &CommandLineState) -> String {
    let shown_input = if command_line.prompt.is_empty() {
        command_line.input.as_str()
    } else {
        &command_line.input[1..]
    };
    format!("{}{}", command_line.prompt, shown_input)
}

fn draw_status_text(frame: &mut Frame, area: Rect, text: &str, bg: Color) {
    let style = Style::default().fg(Text::PRIMARY_FG()).bg(bg);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(TextLine::styled(text.to_string(), style)).style(style),
        area,
    );
}

/// Draws the draw-activity indicator in the status row's last two cells.
fn draw_spinner(frame: &mut Frame, area: Rect, loop_state: LoopState) {
    if area.width < SPINNER_WIDTH || area.height < 1 {
        return;
    }
    let (left, right, color) = draw_activity_glyphs(loop_state);
    let spinner_style = Style::default().fg(color).bg(Surface::APP_BG());
    let spinner_rect = Rect {
        x: area.x + area.width - SPINNER_WIDTH,
        y: area.y,
        width: SPINNER_WIDTH,
        height: 1,
    };
    let text = format!("{left}{right}");
    frame.render_widget(Paragraph::new(text).style(spinner_style), spinner_rect);
}

const STATUS_LEFT_WIDTH: u16 = 12;
const STATUS_RIGHT_WIDTH: u16 = 7;

/// Width of the status spinner.
const SPINNER_WIDTH: u16 = 2;

/// Gap before the status spinner.
const SPINNER_GAP_WIDTH: u16 = 2;

/// Splits the status row into its fixed columns.
fn status_columns(area: Rect) -> [Rect; 5] {
    Layout::horizontal([
        Constraint::Length(STATUS_LEFT_WIDTH),
        Constraint::Min(0),
        Constraint::Length(STATUS_RIGHT_WIDTH),
        Constraint::Length(SPINNER_GAP_WIDTH),
        Constraint::Length(SPINNER_WIDTH),
    ])
    .areas(area)
}

/// Draws the viewer title and memory columns.
fn draw_status_frame(frame: &mut Frame, status_row: Rect, viewer: &dyn ViewerApp, memory_text: &str) {
    let style = Style::default().fg(Text::PRIMARY_FG()).bg(Surface::APP_BG());
    frame.render_widget(Clear, status_row);
    frame.render_widget(ratatui::widgets::Block::default().style(style), status_row);

    let [left, _center, right, _gap, _spinner] = status_columns(status_row);
    frame.render_widget(Paragraph::new(viewer.display_name()).style(style), left);
    frame.render_widget(
        Paragraph::new(memory_text.to_string())
            .style(style)
            .alignment(ratatui::layout::Alignment::Right),
        right,
    );
}

/// Returns the area available to title and command-line text.
fn left_and_center_area(status_row: Rect) -> Rect {
    let [left, center, _right, _gap, _spinner] = status_columns(status_row);
    Rect {
        width: left.width + center.width,
        ..left
    }
}

/// Draws overlays and the status bar after the active viewer.
pub(crate) fn draw(frame: &mut Frame, bar: &mut BarState, view: DrawView<'_>) {
    let DrawView {
        viewer,
        viewer_area,
        status_row,
        loop_state,
        notifications,
        formatted_matches,
        help,
    } = view;
    #[cfg(not(target_arch = "wasm32"))]
    let memory_text = bar.memory.text();
    #[cfg(target_arch = "wasm32")]
    let memory_text = "";
    draw_status_frame(frame, status_row, viewer, memory_text);
    let [_left, center, _right, _gap, _spinner] = status_columns(status_row);
    let slash_line = bar
        .command_line
        .as_ref()
        .filter(|command_line| command_line.input.starts_with('/'));

    if let Some(command_line) = &bar.command_line {
        if !command_line.input.starts_with('/') {
            draw_status_text(
                frame,
                left_and_center_area(status_row),
                &command_line_display_text(command_line),
                Selection::INPUT_BG(),
            );
        }
    } else if let Some(flash) = &bar.flash_message {
        draw_status_text(frame, center, &flash.text, Surface::APP_BG());
    } else if let Some(text) = viewer.status_override() {
        draw_status_text(frame, center, &text, Surface::APP_BG());
    }

    let background = viewer.background_work();
    let (animation_start, tasks) = match background {
        Some(work) => (work.started_at, work.tasks),
        None => (Instant::now(), Vec::new()),
    };

    draw_notifications(frame, notifications, &tasks, spinner_glyph(animation_start));

    draw_spinner(frame, status_row, loop_state);

    if let Some(command_line) = slash_line {
        let matches = if command_line.mode == CommandLineMode::Search {
            formatted_matches
        } else {
            &[]
        };
        let has_results = !matches.is_empty();
        if let Some(panel) = search_popover_area(viewer_area, has_results) {
            draw_search_popover(frame, panel, &command_line.input[1..], matches);
        }
    }

    if let (Some(state), Some(page)) = (&mut bar.help, help) {
        state.visible_rows = draw_help_overlay(frame, viewer_area, &page, state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_tick_clears_an_expired_flash_message() {
        let mut bar = BarState::default();
        bar.flash("test".to_string());
        // Force expiry without sleeping.
        if let Some(flash) = &mut bar.flash_message {
            flash.set_at -= crate::spinner::FLASH_MESSAGE_DURATION;
        }

        assert!(!bar.tick());
        assert!(bar.flash_message.is_none());
    }

    #[test]
    fn bar_tick_keeps_reporting_pending_while_flash_is_fresh() {
        let mut bar = BarState::default();
        bar.flash("test".to_string());
        assert!(bar.tick());
    }

    #[test]
    fn opening_the_command_line_clears_a_pending_flash() {
        let mut bar = BarState::default();
        bar.flash("test".to_string());
        bar.open_command_line(':');
        assert!(bar.flash_message.is_none());
    }
}
