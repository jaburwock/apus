use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Modifier, Style},
    text::Line,
};

use crate::command::protocol::{CommandOutcome, GenomeCommand, HelpContent, ShellCommand};
use crate::keys::Scope;
use crate::style::palette::builtin;
use crate::style::{Selection, Surface, Text};
use crate::viewer::ViewerApp;

#[derive(Clone, Copy)]
enum Action {
    Focus(&'static str),
    Help(HelpContent),
    Tracks,
    CycleColorscheme,
}

const COLORSCHEMES: [(&str, &str); 3] = [
    ("default light", "Light"),
    ("default dark", "Dark"),
    ("red-green dark", "RdGrn"),
];

fn current_scheme() -> Option<usize> {
    let name = builtin::current()?;
    COLORSCHEMES.iter().position(|&(scheme, _)| scheme == name)
}

#[derive(Default)]
pub(super) struct TabBar {
    area: Rect,
    targets: Vec<(Rect, Action)>,
}

impl TabBar {
    pub(super) fn draw(&mut self, frame: &mut Frame, area: Rect, viewers: &[Box<dyn ViewerApp>], active: usize) {
        self.area = area.intersection(frame.area());
        self.targets.clear();
        if self.area.is_empty() {
            return;
        }
        let row = Rect { height: 1, ..self.area };
        let palette_label = current_scheme().map_or("Custom", |i| COLORSCHEMES[i].1);
        let tracks = viewers[active]
            .scopes()
            .contains(&Scope::Genome)
            .then_some(("Tracks", Action::Tracks));
        let buttons = tracks.into_iter().chain([
            (palette_label, Action::CycleColorscheme),
            ("Settings", Action::Help(HelpContent::Settings)),
            ("Hotkeys", Action::Help(HelpContent::Shortcuts)),
        ]);
        let tab_width = viewers
            .iter()
            .map(|viewer| Line::from(viewer.display_name()).width() + 2)
            .sum::<usize>()
            + viewers.len().saturating_sub(1);
        let button_width = buttons.clone().map(|(name, _)| name.len() + 3).sum::<usize>();
        let pad = if tab_width + button_width <= usize::from(row.width) {
            " "
        } else {
            ""
        };
        let mut x = row.x;
        for (index, viewer) in viewers.iter().enumerate() {
            if x == row.right() {
                break;
            }
            let style = if index == active {
                Style::default().fg(Text::PRIMARY_FG()).bg(Selection::ACTIVE_TAB_BG())
            } else {
                Style::default().fg(Text::DIMMED_FG())
            };
            let label = Line::styled(format!("{pad}{}{pad}", viewer.display_name()), style);
            let width = label.width().min(usize::from(row.right() - x)) as u16;
            let bounds = Rect::new(x, row.y, width, 1);
            frame.render_widget(label, bounds);
            self.targets.push((bounds, Action::Focus(viewer.title())));
            x = bounds.right();
            if index + 1 < viewers.len() && x < row.right() {
                frame
                    .buffer_mut()
                    .set_string(x, row.y, "│", Style::default().fg(Text::DIMMED_FG()));
                x += 1;
            }
        }

        let style = Style::default()
            .fg(Text::PRIMARY_FG())
            .bg(Surface::LABEL_BG())
            .add_modifier(Modifier::BOLD);
        let mut required = 0;
        let mut fitted = Vec::new();
        for (name, action) in buttons {
            let width = (name.len() + 2 * pad.len()) as u16;
            if width + 1 > row.right() - x - required {
                break;
            }
            required += width + 1;
            fitted.push((name, action, width));
        }
        let mut x = row.right() - required;
        for (name, action, width) in fitted {
            let bounds = Rect::new(x + 1, row.y, width, 1);
            frame.render_widget(Line::styled(format!("{pad}{name}{pad}"), style), bounds);
            self.targets.push((bounds, action));
            x = bounds.right();
        }
    }

    pub(super) fn contains(&self, cell: Position) -> bool {
        self.area.contains(cell)
    }

    pub(super) fn command_at(&self, cell: Position) -> Option<CommandOutcome> {
        let (_, action) = self.targets.iter().find(|(bounds, _)| bounds.contains(cell))?;
        Some(match *action {
            Action::Focus(title) => ShellCommand::FocusTab(title.into()).into(),
            Action::Help(content) => ShellCommand::OpenHelp(content).into(),
            Action::Tracks => GenomeCommand::OpenTrackPicker.into(),
            Action::CycleColorscheme => {
                let next = current_scheme().map_or(0, |i| (i + 1) % COLORSCHEMES.len());
                ShellCommand::SetColorscheme(COLORSCHEMES[next].0.into()).into()
            }
        })
    }
}
