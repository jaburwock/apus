//! Stateless renderers for notification, search, and help overlays.
//!
//! This module sits beside `bar::draw` as the presentation-only half of the bar:
//! each helper receives a frame and explicit data rather than borrowing `BarState`.
//! Notifications combine live task progress with stored records, keeping progress
//! nearest the status row and newer records nearest the work indicator.
//!
//! The search popover renders the query and ranked [`MatchRow`] values as styled spans,
//! with the first result marked as the selection that `App` will apply on Enter.
//! Its area uses a fixed prompt/result layout, with the taller form selected when
//! results are present.
//!
//! The help popover lists whatever [`HelpPage`] it is given; `App` chooses the rows.
//!
//! These elements draw over the finished viewer frame and reserve no track width,
//! which is the sanctioned overlay case under invariant 9. Neither helper should be
//! moved into viewer layout code.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Flex, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line as TextLine, Span},
    widgets::Paragraph,
};

use super::{HelpPage, HelpState};
use crate::command::protocol::HelpContent;
use crate::command::{HelpEntry, HelpSection};
use crate::notification::{Notification, Severity};
use crate::popover::{ListTitle, Popover, VisibleRows};
use crate::setting::SettingValue;
use crate::style::{Selection, Status, Text};
use crate::viewer::MatchRow;

/// Number of ranked search results shown in the popover.
const SEARCH_RESULT_ROWS: u16 = 5;

const SEARCH_POPOVER_MIN_WIDTH: u16 = 40;
const SEARCH_POPOVER_MAX_WIDTH: u16 = 80;
const SEARCH_POPOVER_PROMPT_HEIGHT: u16 = 3;
const SEARCH_POPOVER_HEIGHT: u16 = SEARCH_RESULT_ROWS + 3;
const SEARCH_POPOVER_UP_ROWS: u16 = 4;

/// Draws one right-aligned line over the frame, truncating from the left so the end of a long
/// message stays visible. Invariant 9 is not in play: this draws *over* content rather than
/// reserving width from any track.
fn draw_overlay_line(frame: &mut Frame, y: u16, text: &str, bg: Color) {
    let frame_area = frame.area();
    let style = Style::default().fg(Text::ON_STATUS_FG()).bg(bg);
    let char_count = text.chars().count();
    let visible_width = char_count.min(frame_area.width as usize);
    let skip = char_count - visible_width;
    let start_x = frame_area.x + frame_area.width - visible_width as u16;
    let buffer = frame.buffer_mut();
    for (col, ch) in text.chars().skip(skip).enumerate() {
        buffer[(start_x + col as u16, y)].set_char(ch).set_style(style);
    }
}

/// Draws in-flight work and notification records bottom-right, over the frame without reserving
/// layout space.
///
/// Progress lines sit closest to the status row and records stack above them, so a record's
/// arrival never shifts a spinner the eye is already tracking. Both are derived per-frame: the
/// records from the shell's stack, the progress lines from live job state. A job is *not* a
/// record — it emitted one when it started, and repeating it every frame is what this split
/// exists to avoid.
pub(super) fn draw_notifications(frame: &mut Frame, records: &[&Notification], tasks: &[String], spinner: char) {
    let frame_area = frame.area();
    // One row for the status row itself, and at least one to draw into.
    if frame_area.height < 2 {
        return;
    }
    let mut y = frame_area.y + frame_area.height - 2;
    let top = frame_area.y;

    for task in tasks.iter().rev() {
        draw_overlay_line(frame, y, &format!("{spinner} {task}"), Status::ONGOING_BG());
        if y == top {
            return;
        }
        y -= 1;
    }

    // Newest nearest the work, so the most recent record is the least buried.
    for record in records.iter().rev() {
        let bg = match record.severity {
            Severity::Info => Status::OK_BG(),
            Severity::Warn => Status::WARN_BG(),
            Severity::Error => Status::ERROR_BG(),
        };
        draw_overlay_line(frame, y, &record.text, bg);
        if y == top {
            return;
        }
        y -= 1;
    }
}

/// Draws the slash command's prompt and any ranked results it receives.
pub(super) fn draw_search_popover(frame: &mut Frame, panel: Rect, query: &str, matches: &[MatchRow]) {
    let popover = Popover::from_rect(panel);
    popover.draw_frame(frame, None, "");

    let inner = popover.inner;
    let prompt = Paragraph::new(format!("Search: {query}"));
    frame.render_widget(prompt, Rect { height: 1, ..inner });

    if matches.is_empty() {
        return;
    }

    let results = Rect {
        y: inner.y + 1,
        height: SEARCH_RESULT_ROWS,
        ..inner
    };
    draw_search_results(frame, results, matches);
}

/// Draws ranked search rows from best to worst.
fn draw_search_results(frame: &mut Frame, area: Rect, matches: &[MatchRow]) {
    let base_style = Style::default();
    let top_style = base_style.bg(Selection::FOCUS_BG()).add_modifier(Modifier::BOLD);
    let rows: [Rect; SEARCH_RESULT_ROWS as usize] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1); SEARCH_RESULT_ROWS as usize])
        .areas(area);

    for (i, row) in rows.iter().enumerate() {
        let is_top = i == 0;
        let style = if is_top { top_style } else { base_style };
        let marker = if is_top {
            "> ".to_string()
        } else {
            format!("{}  ", i + 1)
        };
        let line = match matches.get(i) {
            Some(match_row) => match_row_line(match_row, marker, style),
            None => TextLine::styled(marker, style),
        };
        frame.render_widget(Paragraph::new(line).style(style), *row);
    }
}

/// Builds one search row with highlighted match characters.
fn match_row_line(row: &MatchRow, marker: String, style: Style) -> TextLine<'static> {
    let mut spans = vec![Span::styled(marker, style)];
    let bold_style = style.add_modifier(Modifier::BOLD);
    for (i, ch) in row.text.chars().enumerate() {
        let char_style = if row.bold_char_indices.contains(&(i as u32)) {
            bold_style
        } else {
            style
        };
        spans.push(Span::styled(ch.to_string(), char_style));
    }
    TextLine::from(spans)
}

/// Rows the help popover lists for `sections`: each heading, then its entries. Kept beside
/// [`draw_help_overlay`], which draws the same rows, so the cursor's bound cannot drift from them.
pub(super) fn help_row_count(sections: &[HelpSection]) -> usize {
    help_rows(sections).count()
}

/// The entry drawn at row `index`, or `None` for a heading. Shares [`help_rows`] with the
/// drawing for the same reason as [`help_row_count`].
pub(super) fn help_entry_at(sections: &[HelpSection], index: usize) -> Option<&HelpEntry> {
    help_rows(sections).nth(index)?.1
}

/// Each heading, then its entries, in drawing order.
fn help_rows(sections: &[HelpSection]) -> impl Iterator<Item = (&'static str, Option<&HelpEntry>)> {
    sections.iter().flat_map(|section| {
        std::iter::once((section.heading, None))
            .chain(section.entries.iter().map(|entry| (section.heading, Some(entry))))
    })
}

/// Draws the help popover centred over `area`, or nothing if the area is too small.
pub(super) fn draw_help_overlay(
    frame: &mut Frame,
    area: Rect,
    page: &HelpPage,
    state: &mut HelpState,
) -> Option<VisibleRows> {
    let popover = Popover::centred(area, 80, 80)?;
    let HelpState {
        content, cursor, draft, ..
    } = state;
    let entries = || page.sections.iter().flat_map(|section| &section.entries);
    let key_width = entries().map(|entry| entry.key.chars().count()).max().unwrap_or(0) + 2;
    let value_width = entries()
        .filter_map(setting_text)
        .map(|value| value.len() + 2)
        .max()
        .unwrap_or(0);
    let rows: Vec<_> = help_rows(&page.sections).collect();
    let title = match content {
        HelpContent::Settings => "Settings",
        HelpContent::Shortcuts => "Shortcuts",
    };
    let title = draft
        .as_ref()
        .and_then(|draft| draft.error.as_deref())
        .map_or(ListTitle::WithRange(title), ListTitle::Plain);
    let width = usize::from(popover.inner.width);
    let key_first = key_width + value_width <= width;
    Some(popover.draw_list(
        frame,
        cursor,
        rows.len(),
        title,
        &page.hint,
        |index, selected| match rows[index] {
            (_, Some(entry)) => {
                let label = Span::raw(format!("{:<key_width$}", entry.key));
                let Some(value) = setting_text(entry) else {
                    return TextLine::from(vec![label, Span::raw(entry.description)]);
                };
                let value = match draft.as_ref().filter(|_| selected) {
                    Some(draft) => {
                        let room = if key_first { width - key_width } else { width };
                        let skip = draft.input.chars().count().saturating_sub(room - 1);
                        let start = draft.input.char_indices().nth(skip).map_or(0, |(index, _)| index);
                        Span::styled(
                            format!("{}▏", &draft.input[start..]),
                            Style::default().bg(Selection::INPUT_BG()),
                        )
                    }
                    None => Span::raw(value),
                };
                let spacing = Span::raw(" ".repeat(value_width.saturating_sub(value.width())));
                let description = Span::raw(entry.description);
                if key_first {
                    TextLine::from(vec![label, value, spacing, description])
                } else {
                    TextLine::from(vec![value, spacing, label, description])
                }
            }
            (heading, None) => TextLine::styled(heading, Modifier::BOLD),
        },
    ))
}

fn setting_text(entry: &HelpEntry) -> Option<String> {
    let setting = entry.setting.as_ref()?;
    Some(
        setting
            .value
            .as_ref()
            .map_or_else(|| "Unavailable".to_string(), SettingValue::command_arg),
    )
}

/// Returns the slash popover area, anchored to the full panel's top edge.
pub(super) fn search_popover_area(viewer_area: Rect, has_results: bool) -> Option<Rect> {
    let width = viewer_area.width.min(SEARCH_POPOVER_MAX_WIDTH);
    if width < SEARCH_POPOVER_MIN_WIDTH {
        return None;
    }

    if viewer_area.height < SEARCH_POPOVER_HEIGHT {
        return None;
    }

    let [horizontal] = Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::Center)
        .areas(viewer_area);
    let [vertical] = Layout::vertical([Constraint::Length(SEARCH_POPOVER_HEIGHT)])
        .flex(Flex::Center)
        .areas(horizontal);
    Some(Rect {
        y: vertical.y.saturating_sub(SEARCH_POPOVER_UP_ROWS).max(viewer_area.y),
        height: if has_results {
            SEARCH_POPOVER_HEIGHT
        } else {
            SEARCH_POPOVER_PROMPT_HEIGHT
        },
        ..vertical
    })
}
