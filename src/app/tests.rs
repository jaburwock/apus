//! Tests for the shell in `app.rs`, split out as a child module so they keep private access.
//!
//! Delivery properties drive chords obtained from the keymap. Use typed outcomes for shell behavior.

use super::*;
use crate::basepair::BasePair;
use crate::command::protocol::{CommandOutcome, CommandTarget, GenomeCommand, LineTrigger, TableCommand};
use crate::io::IoWorkerHandle;
use crate::notification::Severity;
use crate::refgenome::IndexedFasta;
use crate::setting::SettingValue;
use crate::viewer::StatusProvider;
use crate::viewer_app::genome::GenomeViewer;
use crate::viewer_app::genome::alignment::AlignmentTrack;
use crate::viewer_app::genome::settings::GenomeViewerSettings;
use std::path::PathBuf;

/// Genome tab active, VCF tab present but backgrounded, so a broadcast or a focus change has a
/// second viewer to reach.
fn new_app() -> App {
    let settings = GenomeViewerSettings::new();
    let io_worker = IoWorkerHandle::spawn();
    let viewer = GenomeViewer::new(
        settings,
        IndexedFasta::hs1_fallback(),
        AlignmentTrack::for_test(),
        None,
        io_worker.clone(),
    );
    let vcf = crate::viewer_app::vcf::VCFViewer::new(io_worker.clone());
    App::new(vec![Box::new(viewer), Box::new(vcf)], io_worker.health())
}

fn apply(app: &mut App, outcome: CommandOutcome) {
    router::route_outcome(app, outcome).expect("outcome and follow-ups should apply");
}

fn press(app: &mut App, chord: Chord) {
    app.on_key(chord.code(), chord.mods());
}

fn run_command(app: &mut App, input: &str) {
    let report = app.run_command(input);
    assert!(report.is_ok(), "{input:?} failed: {:?}", report.failures);
}

#[test]
fn mouse_input_reaches_only_the_exposed_viewer() {
    use crate::input::MouseKind;
    use ratatui::{Terminal, backend::TestBackend};
    use std::cell::Cell;
    use std::rc::Rc;

    struct MouseViewer(&'static str, Rc<Cell<Option<MouseEvent>>>);
    impl CommandTarget for MouseViewer {}
    impl StatusProvider for MouseViewer {}
    impl ViewerApp for MouseViewer {
        fn title(&self) -> &'static str {
            self.0
        }
        fn draw(&mut self, _frame: &mut Frame, _area: Rect, _keymap: &Keymap) {}
        fn tick(&mut self) -> bool {
            false
        }
        fn has_pending_work(&self) -> bool {
            false
        }
        fn on_mouse(&mut self, event: MouseEvent) -> Option<CommandOutcome> {
            self.1.set(Some(event));
            None
        }
    }

    let _palette = palette::lock_for_test();
    let first = Rc::new(Cell::new(None));
    let second = Rc::new(Cell::new(None));
    let io_worker = IoWorkerHandle::spawn();
    let mut app = App::new(
        vec![
            Box::new(MouseViewer("First", Rc::clone(&first))),
            Box::new(MouseViewer("Second", Rc::clone(&second))),
        ],
        io_worker.health(),
    );
    let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
    let mut draw = |app: &mut App| {
        terminal
            .draw(|frame| {
                app.draw(
                    frame,
                    Rect::new(0, 0, 60, 1),
                    Rect::new(0, 1, 60, 19),
                    LoopState::Active,
                )
            })
            .unwrap();
    };
    let mouse = MouseEvent {
        column: 8,
        row: 5,
        kind: MouseKind::LeftDown,
    };
    draw(&mut app);
    app.on_mouse(mouse);
    assert_eq!(first.take(), Some(mouse));

    for (open, close) in [
        (
            ShellCommand::OpenHelp(HelpContent::Settings),
            ShellCommand::Help(ListMove::Close),
        ),
        (
            ShellCommand::OpenLine(LineTrigger::Command),
            ShellCommand::Line(LineEdit::Cancel),
        ),
        (
            ShellCommand::OpenLine(LineTrigger::Slash),
            ShellCommand::Line(LineEdit::Cancel),
        ),
    ] {
        apply(&mut app, open.into());
        draw(&mut app);
        app.on_mouse(mouse);
        assert_eq!(first.take(), None, "input is owned by the overlay");
        apply(&mut app, close.into());
        app.on_mouse(mouse);
        assert_eq!(first.take(), None, "the overlay is still on screen");
        draw(&mut app);
    }

    apply(&mut app, ShellCommand::FocusTab("Second".into()).into());
    app.on_mouse(mouse);
    assert_eq!((first.take(), second.take()), (None, None));
    draw(&mut app);
    app.on_mouse(mouse);
    assert_eq!((first.take(), second.take()), (None, Some(mouse)));
}

#[test]
fn tab_bar_clicks_focus_tabs_and_open_help() {
    use ratatui::{Terminal, backend::TestBackend};

    let _palette = palette::lock_for_test();
    let mut app = new_app();
    let mut terminal = Terminal::new(TestBackend::new(50, 20)).unwrap();
    let draw = |app: &mut App, terminal: &mut Terminal<TestBackend>| {
        terminal
            .draw(|frame| {
                let [bar, content] = crate::runtime::frame_layout(frame.area());
                app.draw(frame, bar, content, LoopState::Active);
            })
            .unwrap();
    };
    let click_at_label = |terminal: &Terminal<TestBackend>, label: &str| -> MouseEvent {
        let buffer = terminal.backend().buffer();
        let row: String = (0..buffer.area.width).map(|x| buffer[(x, 0)].symbol()).collect();
        let column = row.find(label).expect("label rendered on tab bar");
        MouseEvent {
            column: row[..column].chars().count() as u16,
            row: 0,
            kind: MouseKind::LeftDown,
        }
    };

    for viewer in [1, 0] {
        draw(&mut app, &mut terminal);
        let click = click_at_label(&terminal, app.viewers[viewer].display_name());
        for kind in [MouseKind::LeftDrag, MouseKind::LeftUp] {
            app.on_mouse(MouseEvent { kind, ..click });
            assert_ne!(app.active, viewer);
        }
        app.on_mouse(click);
        assert_eq!(app.active, viewer);
    }
    for (label, help) in [("Settings", HelpContent::Settings), ("Hotkeys", HelpContent::Shortcuts)] {
        draw(&mut app, &mut terminal);
        let click = click_at_label(&terminal, label);
        app.on_key(Key::Char(':'), Modifiers::NONE);
        app.on_mouse(click);
        assert!(app.bar.help.is_none());
        app.on_key(Key::Esc, Modifiers::NONE);
        draw(&mut app, &mut terminal);
        app.on_mouse(click);
        assert_eq!(app.bar.help.as_ref().map(|state| state.content), Some(help));
        app.on_key(Key::Esc, Modifiers::NONE);
    }

    draw(&mut app, &mut terminal);
    let settings = click_at_label(&terminal, "Settings");
    terminal.backend_mut().resize(24, 20);
    draw(&mut app, &mut terminal);
    app.on_mouse(settings);
    assert!(app.bar.help.is_none(), "the button did not fit after resize");
    app.on_mouse(click_at_label(&terminal, app.viewers[1].display_name()));
    assert_eq!(app.active, 1);
}

#[test]
fn track_picker_accepts_keys_hover_and_clicks_after_scrolling() {
    use crate::input::MouseKind;
    use ratatui::{Terminal, backend::TestBackend};

    let _palette = palette::lock_for_test();
    let mut app = new_app();
    let features_enabled = |app: &App| {
        app.viewers[0]
            .setting_values()
            .contains(&SettingValue::Featurestrack(true))
    };
    run_command(&mut app, ":set featurestrack false");
    run_command(&mut app, ":tracks");
    assert_eq!(app.scopes()[0], Scope::TrackPicker);

    for _ in 0..7 {
        app.on_key(Key::Down, Modifiers::NONE);
    }
    let mut terminal = Terminal::new(TestBackend::new(80, 8)).unwrap();
    terminal
        .draw(|frame| app.draw(frame, Rect::new(0, 0, 80, 1), Rect::new(0, 1, 80, 7), LoopState::Active))
        .unwrap();
    let mouse = MouseEvent {
        column: 26,
        row: 5,
        kind: MouseKind::Moved,
    };
    app.on_mouse(mouse);
    assert!(!features_enabled(&app));

    app.on_key(Key::Char(' '), Modifiers::NONE);
    assert!(features_enabled(&app));
    app.on_mouse(MouseEvent {
        kind: MouseKind::LeftDown,
        ..mouse
    });
    assert!(!features_enabled(&app));
    assert_eq!(app.scopes()[0], Scope::TrackPicker);
    app.on_key(Key::Esc, Modifiers::NONE);
    assert_eq!(app.scopes()[0], Scope::Genome);
    app.on_key(Key::Char('T'), Modifiers::NONE);
    assert_eq!(app.scopes()[0], Scope::TrackPicker);
}

#[test]
fn an_unrecognised_command_flashes_an_error() {
    let mut app = new_app();

    apply(
        &mut app,
        CommandOutcome::Shell(ShellCommand::OpenLine(LineTrigger::Command)),
    );
    for character in "notacommand".chars() {
        app.bar.type_character(character);
    }
    apply(&mut app, ShellCommand::Line(LineEdit::Submit).into());

    assert!(
        app.bar.command_line.is_none(),
        "the command line must close even on error"
    );
    assert!(
        app.bar.flash_message.is_some(),
        "an unrecognised command must flash an error"
    );
}

#[test]
fn key_only_commands_are_refused_at_the_bar() {
    let mut app = new_app();
    let active = app.active;
    let input = format!(":focustab VCFViewer\n{}", CommandOutcome::JumpToPeer.to_line());

    app.submit_command(&input);

    assert_eq!(app.active, active, "the entire batch must be refused");
    let message = &app
        .bar
        .flash_message
        .as_ref()
        .expect("the bar must report the rejection")
        .text;
    assert!(message.contains("only available through key bindings"));
}

/// Startup replays `:set activevcf` with the genome tab active, so the
/// outcome only lands if it is broadcast rather than sent to the active tab.
#[test]
fn setting_the_active_vcf_reaches_the_vcf_tab_from_another_tab() {
    let settings = GenomeViewerSettings::new();
    let io_worker = IoWorkerHandle::spawn();
    let genome = GenomeViewer::new(
        settings,
        IndexedFasta::hs1_fallback(),
        AlignmentTrack::for_test(),
        None,
        io_worker.clone(),
    );
    let vcf = crate::viewer_app::vcf::VCFViewer::new(io_worker.clone());
    let mut app = App::new(vec![Box::new(genome), Box::new(vcf)], io_worker.health());

    assert_eq!(app.viewers[app.active].title(), "GenomeViewer");
    run_command(&mut app, ":set activevcf testdata/vcf/sample.vcf");

    apply(&mut app, ShellCommand::NextTab.into());
    let status = app.viewers[app.active].status_override().unwrap_or_default();
    assert!(
        status.contains("sample.vcf"),
        "the VCF tab must have received the broadcast outcome, got {status:?}"
    );
}

#[test]
fn an_unclaimed_broadcast_reports_not_applicable() {
    // Genome only: no viewer here owns the VCF.
    let io_worker = IoWorkerHandle::spawn();
    let genome = GenomeViewer::new(
        GenomeViewerSettings::new(),
        IndexedFasta::hs1_fallback(),
        AlignmentTrack::for_test(),
        None,
        io_worker.clone(),
    );
    let mut app = App::new(vec![Box::new(genome)], io_worker.health());

    let report = app.run_command(":set activevcf testdata/vcf/sample.vcf");

    assert!(
        !report.failures.is_empty(),
        "an unclaimed broadcast must report failure"
    );
}

#[test]
fn a_replayed_command_returns_its_error_instead_of_flashing() {
    let mut app = new_app();

    for command in [
        ":set activefasta testdata/definitely_does_not_exist.fa",
        ":set activebam testdata/bam/nope.bam",
        ":set activevcf testdata/vcf/nope.vcf",
    ] {
        let report = app.run_command(command);
        assert!(!report.failures.is_empty(), "{command} must report a failure");
        assert!(
            app.bar.flash_message.is_none(),
            "a replayed error is returned to startup, not flashed to a bar that isn't drawn yet"
        );
    }
}

#[test]
fn a_follow_up_stops_and_notifies_when_its_first_step_fails() {
    use crate::command::protocol::CommandOutcome;
    use crate::viewer::ViewerApp;
    use std::cell::Cell;
    use std::rc::Rc;

    struct GeneratedFailureViewer {
        active_commands: Rc<Cell<usize>>,
    }

    impl CommandTarget for GeneratedFailureViewer {
        fn apply_outcome(&mut self, outcome: CommandOutcome) -> Applied {
            match outcome {
                CommandOutcome::JumpToPeer | CommandOutcome::Genome(GenomeCommand::JumpToFeature { .. }) => {
                    Applied::FollowUps(vec![
                        CommandOutcome::Shell(ShellCommand::FocusTab("Missing".into())),
                        CommandOutcome::Goto {
                            horizontal: Some(BasePair(9)),
                            vertical: None,
                            line: None,
                        },
                    ])
                }
                CommandOutcome::Goto { .. } => {
                    self.active_commands.set(self.active_commands.get() + 1);
                    Applied::Ok
                }
                _ => Applied::NotApplicable,
            }
        }
    }

    impl StatusProvider for GeneratedFailureViewer {}

    impl ViewerApp for GeneratedFailureViewer {
        fn title(&self) -> &'static str {
            "Generated"
        }

        fn draw(&mut self, _frame: &mut Frame, _area: Rect, _keymap: &Keymap) {}

        fn tick(&mut self) -> bool {
            false
        }
        fn has_pending_work(&self) -> bool {
            false
        }
    }

    let active_commands = Rc::new(Cell::new(0));
    let viewer = GeneratedFailureViewer {
        active_commands: Rc::clone(&active_commands),
    };
    let io_worker = IoWorkerHandle::spawn();
    let mut app = App::new(vec![Box::new(viewer)], io_worker.health());

    app.dispatch_interactive(CommandOutcome::JumpToPeer);
    app.submit_command(":nextfeature");

    assert_eq!(active_commands.get(), 0, "a failed focus must stop the jump");
    let severities: Vec<_> = app.notifications.visible().map(|record| record.severity).collect();
    assert_eq!(severities, vec![Severity::Error; 2]);
    assert!(
        app.bar.flash_message.is_none(),
        "a follow-up failure must leave the command bar idle"
    );
}

#[test]
fn a_broadcast_in_a_batch_reaches_every_viewer_for_each_step() {
    use crate::command::protocol::CommandOutcome;
    use crate::viewer::ViewerApp;
    use std::cell::Cell;
    use std::rc::Rc;

    struct BroadcastRecorder(Rc<Cell<usize>>);

    impl CommandTarget for BroadcastRecorder {
        fn apply_outcome(&mut self, outcome: CommandOutcome) -> Applied {
            if matches!(outcome, CommandOutcome::Table(TableCommand::ActiveVcf(_))) {
                self.0.set(self.0.get() + 1);
                Applied::Ok
            } else {
                Applied::NotApplicable
            }
        }
    }

    impl StatusProvider for BroadcastRecorder {}

    impl ViewerApp for BroadcastRecorder {
        fn title(&self) -> &'static str {
            "Recorder"
        }

        fn draw(&mut self, _frame: &mut Frame, _area: Rect, _keymap: &Keymap) {}

        fn tick(&mut self) -> bool {
            false
        }
        fn has_pending_work(&self) -> bool {
            false
        }
    }

    let first = Rc::new(Cell::new(0));
    let second = Rc::new(Cell::new(0));
    let io_worker = IoWorkerHandle::spawn();
    let mut app = App::new(
        vec![
            Box::new(BroadcastRecorder(Rc::clone(&first))),
            Box::new(BroadcastRecorder(Rc::clone(&second))),
        ],
        io_worker.health(),
    );

    let report = app.run_command(":set activevcf testdata/vcf/sample.vcf\n:set activevcf testdata/vcf/sample.vcf.gz");

    assert!(report.is_ok(), "a claimed broadcast must report success");
    assert_eq!(first.get(), 2);
    assert_eq!(second.get(), 2);
}

#[test]
fn submitting_search_applies_the_selected_match_and_closes_the_line() {
    use crate::command::protocol::CommandTarget;
    use crate::search::{MatchTarget, ResolvedMatch};
    use crate::viewer::StatusProvider;
    use std::cell::Cell;
    use std::rc::Rc;

    struct SearchingViewer {
        matches: Vec<ResolvedMatch>,
        entered: Rc<Cell<bool>>,
    }
    impl CommandTarget for SearchingViewer {}
    impl StatusProvider for SearchingViewer {}
    impl ViewerApp for SearchingViewer {
        fn title(&self) -> &'static str {
            "Searching"
        }
        fn draw(&mut self, _frame: &mut Frame, _area: Rect, _keymap: &Keymap) {}
        fn tick(&mut self) -> bool {
            false
        }
        fn has_pending_work(&self) -> bool {
            false
        }
        fn search_matches(&self) -> &[ResolvedMatch] {
            &self.matches
        }
        fn search_enter(&mut self, _selected: &ResolvedMatch) -> Option<String> {
            self.entered.set(true);
            Some("jumped".to_string())
        }
    }

    let entered = Rc::new(Cell::new(false));
    let io_worker = IoWorkerHandle::spawn();
    let viewer = SearchingViewer {
        matches: vec![ResolvedMatch {
            key: "GENE1".into(),
            bold_char_indices: Vec::new(),
            destination: MatchTarget::Row(0),
        }],
        entered: Rc::clone(&entered),
    };
    let mut app = App::new(vec![Box::new(viewer)], io_worker.health());

    apply(
        &mut app,
        CommandOutcome::Shell(ShellCommand::OpenLine(LineTrigger::Slash)),
    );
    app.bar.type_character('G');
    apply(&mut app, ShellCommand::Line(LineEdit::Submit).into());

    assert!(entered.get(), "the selected match must have been applied");
    assert!(app.bar.flash_message.is_some(), "the viewer result must be surfaced");
    assert!(app.bar.command_line.is_none(), "Enter closes the search line");
}

/// Renders the app and returns the frame as text, so a binding's effect is asserted on what was
/// actually rasterised rather than on state one layer below it.
///
/// The status row is excluded: it carries a live memory readout that changes between two
/// otherwise identical renders. No binding targets that row, so dropping it removes noise
/// rather than coverage.
fn rendered(app: &mut App) -> String {
    use ratatui::{Terminal, backend::TestBackend};

    let (width, height) = (80, 24);
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("backend");
    terminal
        .draw(|frame| {
            app.draw(
                frame,
                Rect::new(0, 0, width, 1),
                Rect::new(0, 1, width, height - 1),
                LoopState::Active,
            )
        })
        .expect("draw");
    let buf = terminal.backend().buffer();
    (0..height - 1)
        .map(|y| {
            (0..width)
                .map(|x| buf.cell((x, y)).unwrap().symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A recorder makes dropped viewer commands observable without loading data.
struct BindingViewer {
    scope: Scope,
    busy: bool,
    applied: AppliedCommands,
}

impl CommandTarget for BindingViewer {
    fn apply_outcome(&mut self, outcome: CommandOutcome) -> Applied {
        self.applied.borrow_mut().push(outcome);
        Applied::Ok
    }
}

impl StatusProvider for BindingViewer {
    fn status_override(&self) -> Option<String> {
        Some("viewer readout".to_string())
    }

    fn background_work(&self) -> Option<crate::viewer::BackgroundWork> {
        self.busy.then(|| crate::viewer::BackgroundWork {
            tasks: Vec::new(),
            started_at: std::time::Instant::now(),
        })
    }
}

impl ViewerApp for BindingViewer {
    fn title(&self) -> &'static str {
        "BindingViewer"
    }

    fn draw(&mut self, _frame: &mut Frame, _area: Rect, _keymap: &Keymap) {}

    fn scopes(&self) -> Vec<Scope> {
        vec![self.scope]
    }

    fn tick(&mut self) -> bool {
        false
    }
    fn has_pending_work(&self) -> bool {
        false
    }
}

type AppliedCommands = std::rc::Rc<std::cell::RefCell<Vec<CommandOutcome>>>;

const VIEWER_SCOPES: [Scope; 2] = [Scope::Genome, Scope::Vcf];

fn app_recording_bindings(busy: bool) -> (App, AppliedCommands) {
    let applied = AppliedCommands::default();
    let viewers = VIEWER_SCOPES
        .into_iter()
        .map(|scope| {
            Box::new(BindingViewer {
                scope,
                busy,
                applied: applied.clone(),
            }) as Box<dyn ViewerApp>
        })
        .collect();
    (App::new(viewers, WorkerHealth::dead_for_test()), applied)
}

#[derive(Debug, PartialEq, Eq)]
struct ShellState {
    active: usize,
    quit: bool,
    help: Option<HelpContent>,
    palette_scope: Option<Scope>,
    line: Option<String>,
    flash: bool,
    errors: usize,
}

fn shell_state(app: &App) -> ShellState {
    ShellState {
        active: app.active,
        quit: app.should_quit(),
        help: app.bar.help.as_ref().map(|help| help.content),
        palette_scope: app.palette_pane.as_ref().map(PalettePane::scope),
        line: app.bar.command_line.as_ref().map(|line| line.input.clone()),
        flash: app.bar.flash_message.is_some(),
        errors: app
            .notifications
            .visible()
            .filter(|record| record.severity == Severity::Error)
            .count(),
    }
}

fn reset_binding_app(app: &mut App, applied: &AppliedCommands, active: usize) {
    app.active = active;
    app.quit = false;
    app.bar.help = None;
    app.bar.command_line = None;
    app.palette_pane = None;
    applied.borrow_mut().clear();
}

#[test]
fn every_bound_key_leaves_the_app_as_applying_its_outcome_would() {
    let keymap = Keymap::defaults();
    let (mut by_key, key_commands) = app_recording_bindings(false);
    let (mut by_outcome, typed_commands) = app_recording_bindings(false);
    for (active, scope) in VIEWER_SCOPES.into_iter().enumerate() {
        for binding in keymap.visible(&[scope, Scope::Global]) {
            reset_binding_app(&mut by_key, &key_commands, active);
            reset_binding_app(&mut by_outcome, &typed_commands, active);

            press(&mut by_key, binding.chord);
            apply(&mut by_outcome, binding.command.clone());

            assert_eq!(
                shell_state(&by_key),
                shell_state(&by_outcome),
                "{scope:?} {}",
                binding.chord
            );
            assert_eq!(
                *key_commands.borrow(),
                *typed_commands.borrow(),
                "{scope:?} {}",
                binding.chord
            );
        }
    }
}

#[test]
fn no_bound_key_fires_while_the_bar_is_taking_input() {
    let keymap = Keymap::defaults();
    let line_bindings = keymap.visible(&[Scope::CommandLine]);
    let (mut app, applied) = app_recording_bindings(false);
    let (mut expected, _) = app_recording_bindings(false);
    for (active, scope) in VIEWER_SCOPES.into_iter().enumerate() {
        app.active = active;
        expected.active = active;
        for binding in keymap.visible(&[scope, Scope::Global]) {
            for app in [&mut app, &mut expected] {
                apply(app, ShellCommand::OpenLine(LineTrigger::Command).into());
            }
            if let Some(line_binding) = line_bindings.iter().find(|line| line.chord == binding.chord) {
                apply(&mut expected, line_binding.command.clone());
            } else if let Key::Char(character) = binding.chord.code() {
                expected.bar.type_character(character);
            }

            press(&mut app, binding.chord);

            assert_eq!(shell_state(&app), shell_state(&expected), "{scope:?} {}", binding.chord);
            assert!(applied.borrow().is_empty(), "{scope:?} {}", binding.chord);
        }
    }
}

#[test]
fn no_blocked_key_acts_while_background_work_is_in_flight() {
    let keymap = Keymap::defaults();
    let (mut app, applied) = app_recording_bindings(true);
    let (mut expected, expected_applied) = app_recording_bindings(true);
    for (active, scope) in VIEWER_SCOPES.into_iter().enumerate() {
        for binding in keymap.visible(&[scope, Scope::Global]) {
            reset_binding_app(&mut app, &applied, active);
            reset_binding_app(&mut expected, &expected_applied, active);
            if !binding
                .command
                .descriptor()
                .expect("bound commands have descriptors")
                .blocked_while_busy
            {
                apply(&mut expected, binding.command.clone());
            }

            press(&mut app, binding.chord);

            assert_eq!(shell_state(&app), shell_state(&expected), "{scope:?} {}", binding.chord);
            assert_eq!(
                *applied.borrow(),
                *expected_applied.borrow(),
                "{scope:?} {}",
                binding.chord
            );
        }
    }

    reset_binding_app(&mut app, &applied, 0);
    apply(&mut app, ShellCommand::OpenPalettePane.into());
    let scope = app.palette_pane.as_ref().unwrap().scope();
    let close = CommandOutcome::Shell(ShellCommand::Palette(PaneCommand::Close));
    let chord = keymap
        .keys_for(scope, &close)
        .into_iter()
        .next()
        .expect("pane close binding");
    press(&mut app, chord);
    assert!(app.palette_pane.is_none());
}

#[test]
fn an_open_overlay_captures_bound_keys() {
    struct RecordingGenome(GenomeViewer, AppliedCommands);
    impl CommandTarget for RecordingGenome {
        fn apply_outcome(&mut self, outcome: CommandOutcome) -> Applied {
            self.1.borrow_mut().push(outcome.clone());
            self.0.apply_outcome(outcome)
        }
    }
    impl StatusProvider for RecordingGenome {}
    impl ViewerApp for RecordingGenome {
        fn title(&self) -> &'static str {
            self.0.title()
        }
        fn scopes(&self) -> Vec<Scope> {
            self.0.scopes()
        }
        fn draw(&mut self, _frame: &mut Frame, _area: Rect, _keymap: &Keymap) {}
        fn tick(&mut self) -> bool {
            false
        }
        fn has_pending_work(&self) -> bool {
            false
        }
    }

    let io_worker = IoWorkerHandle::spawn();
    let applied = AppliedCommands::default();
    let genome = GenomeViewer::new(
        GenomeViewerSettings::new(),
        IndexedFasta::hs1_fallback(),
        AlignmentTrack::for_test(),
        None,
        io_worker.clone(),
    );
    let vcf = crate::viewer_app::vcf::VCFViewer::new(io_worker.clone());
    let mut app = App::new(
        vec![Box::new(RecordingGenome(genome, applied.clone())), Box::new(vcf)],
        io_worker.health(),
    );

    let keymap = Keymap::defaults();
    let overlay = keymap.visible(&[Scope::ContigSelector]);
    for binding in keymap.visible(&[Scope::Genome, Scope::Global]) {
        apply(&mut app, GenomeCommand::OpenContigSelector.into());
        applied.borrow_mut().clear();
        let before = shell_state(&app);
        let selected = overlay.iter().find(|inner| inner.chord == binding.chord);

        press(&mut app, binding.chord);

        assert_eq!(shell_state(&app), before, "{}", binding.chord);
        assert_eq!(
            applied.borrow().iter().collect::<Vec<_>>(),
            selected.map(|binding| &binding.command).as_slice(),
            "{}",
            binding.chord,
        );
    }
}

#[test]
fn help_shortcuts_open_from_either_view_and_close_keys_dismiss_help() {
    let (mut app, _) = app_recording_bindings(false);
    for active in 0..VIEWER_SCOPES.len() {
        app.active = active;
        for (open, help) in [(',', HelpContent::Settings), ('?', HelpContent::Shortcuts)] {
            for close in [Key::Char('q'), Key::Esc, Key::Char('?')] {
                app.on_key(Key::Char(open), Modifiers::NONE);
                assert_eq!(shell_state(&app).help, Some(help), "{active}");

                app.on_key(close, Modifiers::NONE);
                let state = shell_state(&app);
                assert_eq!((state.help, state.quit), (None, false), "{active} {close}");
            }
        }
    }
}

/// The popover lists the stack beneath it, not its own scope: the keys in its rendered rows are
/// exactly the genome view's reachable chords, each once.
#[test]
fn shortcuts_list_the_genome_views_reachable_keys_and_none_of_helps_own() {
    use ratatui::{Terminal, backend::TestBackend};

    let mut app = new_app();
    app.on_key(Key::Char('?'), Modifiers::NONE);
    // Tall enough that the whole list fits without scrolling.
    let (width, height) = (100, 50);
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("backend");
    terminal
        .draw(|frame| {
            app.draw(
                frame,
                Rect::new(0, 0, width, 1),
                Rect::new(0, 1, width, height - 1),
                LoopState::Active,
            )
        })
        .expect("draw");
    let buf = terminal.backend().buffer();
    let cells: Vec<Vec<&str>> = (0..height)
        .map(|y| (0..width).map(|x| buf.cell((x, y)).unwrap().symbol()).collect())
        .collect();

    let reachable = [Scope::Genome, Scope::Global];
    let headings = reachable.map(Scope::heading);
    // The popover's first row is the genome heading; its rows run down to the bottom border.
    let (top, left) = cells
        .iter()
        .enumerate()
        .find_map(|(y, row)| {
            (1..row.len())
                .find(|&x| row[x..].concat().starts_with(headings[0]))
                .map(|x| (y, x))
        })
        .expect("the genome heading is drawn");
    let mut keys: Vec<String> = cells[top..]
        .iter()
        .take_while(|row| row[left - 1] == "│")
        .map(|row| {
            row[left..]
                .iter()
                .take_while(|cell| **cell != "│")
                .copied()
                .collect::<String>()
        })
        .filter(|row| !headings.contains(&row.trim_end()))
        .flat_map(|row| {
            let key_column = row.split("  ").next().unwrap_or_default().to_string();
            key_column.split_whitespace().map(str::to_string).collect::<Vec<_>>()
        })
        .collect();
    let mut expected: Vec<String> = Keymap::defaults()
        .visible(&reachable)
        .iter()
        .map(|binding| binding.chord.to_string())
        .collect();
    keys.sort();
    expected.sort();

    assert_eq!(keys, expected);
}

/// The value is read from the owning viewer even while another tab is focused.
#[test]
fn help_shows_a_settings_live_value_from_a_background_tab() {
    let mut app = new_app();
    run_command(&mut app, ":set movemid 77\n:focustab VCFViewer\n:help");

    let value = app
        .help_sections()
        .into_iter()
        .flat_map(|section| section.entries)
        .find(|entry| entry.key == "set movemid")
        .and_then(|entry| entry.setting?.value);
    assert_eq!(value, Some(SettingValue::Movemid(BasePair(77))));
}

#[test]
fn enter_in_settings_prefills_the_selected_command_and_ignores_headings() {
    let mut app = new_app();
    run_command(&mut app, ":settings");
    assert_eq!(app.bar.help.as_ref().unwrap().cursor.selected(), 1);
    app.on_key(Key::Up, Modifiers::NONE);
    app.on_key(Key::Enter, Modifiers::NONE);
    let state = shell_state(&app);
    assert_eq!((state.help, state.line), (Some(HelpContent::Settings), None));

    select_settings_row(&mut app, "set colorscheme");
    app.on_key(Key::Enter, Modifiers::NONE);
    let state = shell_state(&app);
    assert_eq!((state.help, state.line.as_deref()), (None, Some(":set colorscheme ")));
}

fn select_settings_row(app: &mut App, key: &str) -> usize {
    let row = app
        .help_sections()
        .iter()
        .flat_map(|section| std::iter::once(None).chain(section.entries.iter().map(Some)))
        .position(|entry| entry.is_some_and(|entry| entry.key == key))
        .unwrap_or_else(|| panic!("{key} is listed"));
    let current = app.bar.help.as_ref().unwrap().cursor.selected();
    let movement = if current < row { Key::Down } else { Key::Up };
    for _ in 0..current.abs_diff(row) {
        app.on_key(movement, Modifiers::NONE);
    }
    assert_eq!(app.bar.help.as_ref().unwrap().cursor.selected(), row);
    row
}

fn type_setting_value(app: &mut App, value: &str) {
    for character in value.chars() {
        app.on_key(Key::Char(character), Modifiers::NONE);
    }
}

#[test]
fn settings_toggle_reaches_the_background_owner_and_applies_its_side_effects() {
    let mut app = new_app();
    run_command(&mut app, ":set indellength true\n:focustab VCFViewer\n:settings");

    let row = select_settings_row(&mut app, "set readlength");
    app.on_key(Key::Enter, Modifiers::NONE);
    let values = app.viewers[0].setting_values();
    assert!(values.contains(&SettingValue::Readlength(true)));
    assert!(values.contains(&SettingValue::Indellength(false)));
    let help = app.bar.help.as_ref().unwrap();
    assert!(help.draft.is_none());
    assert_eq!(help.cursor.selected(), row);
    assert_eq!(app.viewers[app.active].title(), "VCFViewer");
}

#[test]
fn settings_hover_and_click_act_on_the_rendered_row_after_scrolling() {
    use ratatui::{Terminal, backend::TestBackend};

    let _palette = palette::lock_for_test();
    let mut app = new_app();
    run_command(
        &mut app,
        ":set indellength true\n:set allelefraction 30\n:set featurestrack false\n:focustab VCFViewer\n:settings",
    );
    let (width, height) = (80, 12);
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let mut mouse_at = |app: &mut App, label: &str| {
        terminal
            .draw(|frame| {
                app.draw(
                    frame,
                    Rect::new(0, 0, width, 1),
                    Rect::new(0, 1, width, height - 1),
                    LoopState::Active,
                )
            })
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .chunks(usize::from(width))
            .enumerate()
            .find_map(|(row, cells)| {
                let column = cells
                    .windows(label.len())
                    .position(|cells| cells.iter().flat_map(|cell| cell.symbol().bytes()).eq(label.bytes()))?;
                Some(MouseEvent {
                    column: column as u16,
                    row: row as u16,
                    kind: MouseKind::LeftDown,
                })
            })
            .unwrap_or_else(|| panic!("{label} is visible"))
    };

    let numeric_row = select_settings_row(&mut app, "set allelefraction");
    select_settings_row(&mut app, "set featurestrack");
    let mouse = mouse_at(&mut app, "set allelefraction");
    app.on_mouse(MouseEvent {
        kind: MouseKind::Moved,
        ..mouse
    });
    let help = app.bar.help.as_ref().unwrap();
    assert_eq!(help.cursor.selected(), numeric_row);
    assert!(help.draft.is_none());
    app.on_mouse(mouse);
    let draft = app.bar.help.as_ref().unwrap().draft.as_ref().unwrap();
    assert_eq!(draft.input, "30");
    type_setting_value(&mut app, "50");
    app.on_mouse(MouseEvent {
        row: mouse.row + 1,
        kind: MouseKind::Moved,
        ..mouse
    });
    let help = app.bar.help.as_ref().unwrap();
    assert_eq!(help.cursor.selected(), numeric_row);
    assert_eq!(help.draft.as_ref().unwrap().input, "50");
    app.on_mouse(mouse);
    assert_eq!(app.bar.help.as_ref().unwrap().draft.as_ref().unwrap().input, "50");
    let help = app.bar.help.as_ref().unwrap();
    let border = (0..width)
        .find(|&x| help.contains((x, mouse.row).into()))
        .expect("help has a border");
    app.on_mouse(MouseEvent {
        column: border,
        ..mouse
    });
    assert!(app.bar.help.as_ref().unwrap().draft.is_none());
    assert!(
        app.viewers[0]
            .setting_values()
            .contains(&SettingValue::Allelefraction(30))
    );

    app.on_mouse(mouse);
    type_setting_value(&mut app, "50");
    app.on_key(Key::Enter, Modifiers::NONE);
    assert!(
        app.viewers[0]
            .setting_values()
            .contains(&SettingValue::Allelefraction(50))
    );
    assert!(app.bar.help.as_ref().unwrap().draft.is_none());

    app.on_mouse(mouse);
    type_setting_value(&mut app, "70");
    let mouse = mouse_at(&mut app, "set featurestrack");
    app.on_mouse(mouse);
    assert!(app.bar.help.as_ref().unwrap().draft.is_none());
    let values = app.viewers[0].setting_values();
    assert!(values.contains(&SettingValue::Allelefraction(50)));
    assert!(values.contains(&SettingValue::Featurestrack(true)));

    select_settings_row(&mut app, "set indellength");
    let mouse = mouse_at(&mut app, "set readlength");
    for kind in [MouseKind::LeftDown, MouseKind::LeftDrag, MouseKind::LeftUp] {
        app.on_mouse(MouseEvent { kind, ..mouse });
    }
    let values = app.viewers[0].setting_values();
    assert!(values.contains(&SettingValue::Readlength(true)));
    assert!(values.contains(&SettingValue::Indellength(false)));
}

#[test]
fn settings_edit_accepts_command_value_syntax() {
    use crate::setting::ReadColor;

    let mut app = new_app();
    run_command(
        &mut app,
        ":set movemid 77\n:set readcolor lane\n:focustab VCFViewer\n:settings",
    );
    for (key, before, input, after) in [
        ("set movemid", "77", "2k", SettingValue::Movemid(BasePair(2_000))),
        (
            "set readcolor",
            "lane",
            "direction",
            SettingValue::Readcolor(ReadColor::Direction),
        ),
    ] {
        let row = select_settings_row(&mut app, key);
        app.on_key(Key::Enter, Modifiers::NONE);
        let draft = app.bar.help.as_ref().unwrap().draft.as_ref().unwrap();
        assert_eq!(draft.input, before);
        type_setting_value(&mut app, input);
        app.on_key(Key::Enter, Modifiers::NONE);
        assert!(app.viewers[0].setting_values().contains(&after));
        let help = app.bar.help.as_ref().unwrap();
        assert!(help.draft.is_none());
        assert_eq!(help.cursor.selected(), row);
    }
}

#[test]
fn settings_edit_keeps_rejected_input_for_correction_and_can_cancel() {
    let mut app = new_app();
    run_command(&mut app, ":set allelefraction 30\n:focustab VCFViewer\n:settings");
    let row = select_settings_row(&mut app, "set allelefraction");
    app.on_key(Key::Enter, Modifiers::NONE);
    type_setting_value(&mut app, "500");
    app.on_key(Key::Enter, Modifiers::NONE);
    let draft = app.bar.help.as_ref().unwrap().draft.as_ref().unwrap();
    assert_eq!(draft.input, "500");
    assert!(draft.error.is_some());
    assert!(
        app.viewers[0]
            .setting_values()
            .contains(&SettingValue::Allelefraction(30))
    );

    app.on_key(Key::Backspace, Modifiers::NONE);
    app.on_key(Key::Enter, Modifiers::NONE);
    assert!(
        app.viewers[0]
            .setting_values()
            .contains(&SettingValue::Allelefraction(50))
    );
    assert!(app.bar.help.as_ref().unwrap().draft.is_none());

    app.on_key(Key::Enter, Modifiers::NONE);
    type_setting_value(&mut app, "60");
    app.on_key(Key::Esc, Modifiers::NONE);
    assert!(
        app.viewers[0]
            .setting_values()
            .contains(&SettingValue::Allelefraction(50))
    );
    let help = app.bar.help.as_ref().unwrap();
    assert!(help.draft.is_none());
    assert_eq!(help.cursor.selected(), row);
}

#[test]
fn app_tick_settles_once_viewer_activity_stops() {
    let mut app = new_app();
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2500);
    let mut animating = true;
    while animating && std::time::Instant::now() < deadline {
        animating = app.tick();
        std::thread::sleep(std::time::Duration::from_millis(16));
    }
    assert!(
        !animating,
        "App::tick() must settle to false once viewer/job activity stops"
    );
}

/// An async failure in a background tab is logged as an Error on `tick()`, before any `draw()`,
/// and displayed only once that tab is focused.
#[test]
fn a_background_viewer_error_is_logged_at_once_and_shown_on_focus() {
    let mut app = new_app();
    // A missing GTF cache: a real asynchronous failure path, not a hand-built record.
    run_command(&mut app, ":load testdata/no_such.gtf\n:focustab VCFViewer");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !app.has_errors() {
        assert!(
            std::time::Instant::now() < deadline,
            "the background GTF failure did not reach the log in time"
        );
        app.tick();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(
        app.diagnostics()
            .iter()
            .any(|entry| entry.severity == Severity::Error && entry.message.contains("Failed to load GTF")),
        "got {:?}",
        app.diagnostics()
    );
    assert_eq!(
        app.notifications.visible().count(),
        0,
        "a background record waits for focus"
    );

    run_command(&mut app, ":focustab GenomeViewer");
    app.tick();
    let severities: Vec<_> = app.notifications.visible().map(|record| record.severity).collect();
    assert_eq!(severities, vec![Severity::Error]);
}

#[test]
fn a_notification_preserves_the_status_rows_coordinate_readout() {
    use ratatui::{Terminal, backend::TestBackend};

    struct ReadoutViewer;
    impl CommandTarget for ReadoutViewer {}
    impl StatusProvider for ReadoutViewer {
        fn status_override(&self) -> Option<String> {
            Some("chr1:1,234,567".to_string())
        }
    }
    impl ViewerApp for ReadoutViewer {
        fn title(&self) -> &'static str {
            "Readout"
        }
        fn draw(&mut self, _frame: &mut Frame, _area: Rect, _keymap: &Keymap) {}
        fn tick(&mut self) -> bool {
            false
        }
        fn has_pending_work(&self) -> bool {
            false
        }
    }

    let io_worker = IoWorkerHandle::spawn();
    let mut app = App::new(vec![Box::new(ReadoutViewer)], io_worker.health());
    app.notifications
        .push(Notification::new(Severity::Error, "load failed".to_string()));
    let mut terminal = Terminal::new(TestBackend::new(60, 20)).expect("backend");
    terminal
        .draw(|frame| {
            app.draw(
                frame,
                Rect::new(0, 0, 60, 1),
                Rect::new(0, 1, 60, 19),
                LoopState::Active,
            )
        })
        .expect("draw");
    let row: String = (0..60)
        .map(|x| terminal.backend().buffer().cell((x, 19)).unwrap().symbol())
        .collect();
    assert!(
        row.contains("chr1:1,234,567"),
        "a notification must not cover the coordinate readout"
    );
}

#[test]
fn opening_the_palette_pane_gives_the_viewer_a_separate_narrower_rect() {
    use ratatui::{Terminal, backend::TestBackend};
    use std::cell::Cell;
    use std::rc::Rc;

    struct AreaViewer(Rc<Cell<Option<Rect>>>);
    impl CommandTarget for AreaViewer {}
    impl StatusProvider for AreaViewer {}
    impl ViewerApp for AreaViewer {
        fn title(&self) -> &'static str {
            "Area"
        }
        fn draw(&mut self, _frame: &mut Frame, area: Rect, _keymap: &Keymap) {
            self.0.set(Some(area));
        }
        fn tick(&mut self) -> bool {
            false
        }
        fn has_pending_work(&self) -> bool {
            false
        }
    }

    let area = Rc::new(Cell::new(None));
    let io_worker = IoWorkerHandle::spawn();
    let mut app = App::new(vec![Box::new(AreaViewer(Rc::clone(&area)))], io_worker.health());
    let (width, height) = (PANE_WIDTH * 3, 20);
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("backend");
    let tabs = Rect::new(0, 0, width, 1);
    let content = Rect::new(0, 1, width, height - 1);

    terminal
        .draw(|frame| app.draw(frame, tabs, content, LoopState::Active))
        .expect("draw");
    let full_viewer = area.get().unwrap();
    assert_eq!(full_viewer.x, content.x);
    assert_eq!(full_viewer.width, content.width);

    run_command(&mut app, ":palette");
    terminal
        .draw(|frame| app.draw(frame, tabs, content, LoopState::Active))
        .expect("draw");
    assert_eq!(
        area.get().unwrap(),
        Rect::new(
            full_viewer.x + PANE_WIDTH,
            full_viewer.y,
            full_viewer.width - PANE_WIDTH,
            full_viewer.height,
        )
    );
    assert!(
        (0..PANE_WIDTH).any(|x| terminal.backend().buffer().cell((x, content.y)).unwrap().symbol() != " "),
        "the pane must paint in the columns it occupies"
    );
}

/// The worker's death is a state, not an event, so the shell observes it on `tick()` and never
/// restates it on subsequent ticks or frames.
#[test]
fn a_dead_worker_is_reported_once_however_many_frames_pass() {
    let mut app = new_app();
    app.io_worker_health = Some(WorkerHealth::dead_for_test());

    for _ in 0..5 {
        app.tick();
    }

    let reports: Vec<_> = app.notifications.visible().collect();
    assert_eq!(reports.len(), 1, "the same death must not be restated every frame");
    assert_eq!(reports[0].severity, Severity::Error);
    assert!(app.has_errors());
    assert_eq!(app.diagnostics().len(), 1);
}

/// Genome tab plus a VCF tab holding the committed fixture, focused on the VCF.
fn app_with_loaded_vcf() -> App {
    use crate::viewer_app::vcf::VCFViewer;

    let io_worker = IoWorkerHandle::spawn();
    let genome = GenomeViewer::new(
        GenomeViewerSettings::new(),
        IndexedFasta::hs1_fallback(),
        AlignmentTrack::for_test(),
        None,
        io_worker.clone(),
    );
    let mut vcf = VCFViewer::new(io_worker.clone());
    vcf.apply_outcome(TableCommand::ActiveVcf(PathBuf::from("testdata/vcf/sample.vcf")).into())
        .into_target_result()
        .expect("the fixture must load");

    let mut app = App::new(vec![Box::new(genome), Box::new(vcf)], io_worker.health());
    app.active = 1;
    while app.tick() {}
    app
}

#[test]
fn a_peer_jump_from_the_vcf_keeps_its_contig_and_coordinate() {
    let mut app = app_with_loaded_vcf();
    let (target_contig, target_position) = last_fixture_variant();

    // Walk the cursor to the last row, which the fixture puts on a different contig.
    for _ in 0..10 {
        use crate::command::protocol::{Direction, ScrollStep};
        apply(
            &mut app,
            CommandOutcome::Scroll {
                direction: Direction::Down,
                step: ScrollStep::Near,
            },
        );
    }
    while app.tick() {}

    apply(&mut app, CommandOutcome::JumpToPeer);
    while app.tick() {}

    let screen = rendered(&mut app);
    assert!(
        screen.contains(&format!("g.{}", BasePair(target_position).format_with_commas())),
        "the jump must reach {target_contig}:{target_position} — got:\n{screen}"
    );
    assert!(matches!(
        app.viewers[0].apply_outcome(CommandOutcome::JumpToPeer),
        Applied::FollowUps(outcomes)
            if matches!(outcomes.as_slice(), [CommandOutcome::Shell(ShellCommand::FocusTab(_)), CommandOutcome::Table(TableCommand::GotoLocus { contig, .. })]
                if contig == &target_contig)
    ));
}

#[test]
fn enter_selects_a_bam_carrying_the_variant_and_jumps_to_its_locus() {
    let mut app = new_app();
    let dir = "testdata/hg002/multisample";
    run_command(
        &mut app,
        &format!(
            ":set activefasta {dir}/hg38-multisample.fa.gz\n\
             :set chromosometrack false\n\
             :set localareatrack false\n\
             :set activebam {dir}/del2477.bam\n\
             :addbam {dir}/ins975.bam\n\
             :addbam {dir}/ins55.bam\n\
             :addbam {dir}/ins55_rep.bam\n\
             :set activevcf {dir}/hg002-multisample.vcf.gz\n\
             :focustab VCFViewer\n\
             :goto 3"
        ),
    );
    let settle = |app: &mut App| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while app.has_pending_work() {
            assert!(std::time::Instant::now() < deadline, "fixture work did not settle");
            app.tick();
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(!app.has_errors(), "{:?}", app.diagnostics());
    };
    settle(&mut app);

    app.on_key(Key::Enter, Modifiers::NONE);
    assert_eq!(app.active_title(), "VCFViewer");
    assert!(app.scopes().contains(&Scope::BamSwitcher));
    app.on_key(Key::Esc, Modifiers::NONE);
    assert_eq!(app.active_title(), "VCFViewer");
    assert!(!app.scopes().contains(&Scope::BamSwitcher));
    assert!(rendered(&mut app).contains("Current line: 3"));

    app.on_key(Key::Enter, Modifiers::NONE);
    settle(&mut app);
    let menu = rendered(&mut app);
    assert!(menu.contains("Current line: 3"));
    assert!(menu.contains("ins55.bam"));
    assert!(menu.contains("ins55_rep.bam"));
    assert!(!menu.contains("ins975.bam"));

    app.on_key(Key::Down, Modifiers::NONE);
    app.on_key(Key::Enter, Modifiers::NONE);
    settle(&mut app);
    assert_eq!(app.active_title(), "GenomeViewer");
    assert!(!app.scopes().contains(&Scope::BamSwitcher));
    let screen = rendered(&mut app);
    assert!(screen.contains("ins55_rep.bam"), "{screen}");
    assert!(screen.contains("g.130,963,905"), "{screen}");
    assert!(matches!(
        app.viewers[0].apply_outcome(CommandOutcome::JumpToPeer),
        Applied::FollowUps(outcomes)
            if matches!(outcomes.as_slice(), [_, CommandOutcome::Table(TableCommand::GotoLocus { contig, .. })]
                if contig == "chr2")
    ));
}

/// The reverse jump is checked at the app's raster seam: the genome command
/// is routed, the asynchronous lookup lands the VCF cursor, and the frame
/// shows the resulting 1-based row number.
#[test]
fn a_peer_jump_from_the_genome_lands_on_the_nearest_vcf_row() {
    let mut app = app_with_loaded_vcf();
    app.active = 0;
    run_command(&mut app, ":set activecontig chr1\n:goto 500");
    let expected_row = nearest_fixture_row(500);

    apply(&mut app, CommandOutcome::JumpToPeer);
    while app.tick() {}

    assert_eq!(app.active_title(), "VCFViewer");
    let screen = rendered(&mut app);
    assert!(
        screen.contains(&format!("Current line: {}", expected_row + 1)),
        "the reverse jump must land on row {} — got:\n{screen}",
        expected_row + 1
    );
}

fn nearest_fixture_row(display_position: u64) -> usize {
    let target = BasePair::from_display(display_position);
    crate::viewer_app::vcf::ingest::read_preview(std::path::Path::new("testdata/vcf/sample.vcf"), usize::MAX)
        .expect("fixture readable")
        .rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.contig == "chr1")
        .min_by_key(|(_, row)| (row.position.as_u64().abs_diff(target.as_u64()), row.position.as_u64()))
        .map(|(row, _)| row)
        .expect("fixture has variants")
}

/// The last data line of the fixture, as `(contig, 1-based position)`.
fn last_fixture_variant() -> (String, u64) {
    let text = std::fs::read_to_string("testdata/vcf/sample.vcf").expect("fixture readable");
    let line = text
        .lines()
        .rfind(|line| !line.starts_with('#'))
        .expect("the fixture has a data line");
    let mut fields = line.split('\t');
    let contig = fields.next().expect("CHROM").to_string();
    let position = fields.next().expect("POS").parse().expect("POS is a number");
    (contig, position)
}

/// Multiple viewer records queued before one tick must all survive into both the log and stack.
#[test]
fn several_records_in_one_tick_all_reach_the_shell() {
    let mut app = new_app();
    for forward in [true, false] {
        apply(&mut app, GenomeCommand::JumpToFeature { forward }.into());
    }

    assert!(app.tick(), "queued viewer notifications must request a redraw");

    assert_eq!(app.notifications.visible().count(), 2, "both records must be kept");
    assert_eq!(app.diagnostics().len(), 2, "both records must be logged");
    assert!(!app.has_errors(), "warnings alone must not mark the session errored");
}

#[test]
fn diagnostic_log_retains_records_after_timed_notifications_expire() {
    use crate::notification::Lifetime;
    use std::time::Duration;

    let mut app = new_app();
    app.record(Notification::new(Severity::Info, "saved".into()).with_lifetime(Lifetime::Timed(Duration::ZERO)));

    assert!(!app.notifications.tick(), "the timed notification must expire");
    assert_eq!(app.notifications.visible().count(), 0);
    assert_eq!(app.diagnostics().len(), 1);
    assert_eq!(app.diagnostics()[0].message, "saved");
}

/// A caller waiting on `has_pending_work` must not finish while a background tab is still loading,
/// and `tick` must carry that load to completion without the tab being focused.
#[test]
fn a_background_load_is_pending_until_ticked_to_completion() {
    let mut app = new_app();
    // The VCF tab indexes in the background; the genome tab stays focused.
    run_command(&mut app, ":set activevcf testdata/vcf/sample.vcf");
    assert!(app.has_pending_work(), "a background index build is pending work");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while app.has_pending_work() {
        assert!(
            std::time::Instant::now() < deadline,
            "the background load did not settle"
        );
        app.tick();
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}
