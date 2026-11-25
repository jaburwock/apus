//! Terminal entry point and Crossterm event loop.
//!
//! This binary owns terminal setup and teardown, Crossterm input translation, and the terminal
//! event loop. Shared startup composition, chrome layout, and redraw-cadence decisions live in
//! `apus` so other entry points reuse them verbatim.
//!
//! The loop drains queued input before drawing, bounded by `MAX_EVENTS_PER_FRAME`, so key-repeat
//! bursts collapse into the current frame without starving rendering. It blocks once settled and
//! polls while the settling deadline is active. Keep terminal lifecycle here; viewers receive
//! events, ticks, and draw calls without knowing how the frontend drives them.

use std::io;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::{CommandFactory, FromArgMatches};
use crossterm::{
    event::{
        self, DisableFocusChange, DisableMouseCapture, EnableFocusChange, EnableMouseCapture, Event, KeyEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use apus::{
    ANIMATION_FRAME_INTERVAL, Cli, DRAW_ACTIVITY_WINDOW, Key, Modifiers, MouseEvent, MouseKind,
    advance_settle_deadline, build_app, capture, drain_should_stop, frame_layout, should_poll_rather_than_block,
};

/// Translates a crossterm key code into the library's backend-agnostic [`Key`].
fn translate_key(code: event::KeyCode) -> Option<Key> {
    Some(match code {
        event::KeyCode::Char(character) => Key::Char(character),
        event::KeyCode::Left => Key::Left,
        event::KeyCode::Right => Key::Right,
        event::KeyCode::Up => Key::Up,
        event::KeyCode::Down => Key::Down,
        event::KeyCode::PageUp => Key::PageUp,
        event::KeyCode::PageDown => Key::PageDown,
        event::KeyCode::Enter => Key::Enter,
        event::KeyCode::Tab => Key::Tab,
        event::KeyCode::BackTab => Key::BackTab,
        event::KeyCode::Esc => Key::Esc,
        event::KeyCode::Backspace => Key::Backspace,
        event::KeyCode::F(n) => Key::F(n),
        _ => return None,
    })
}

fn translate_modifiers(mods: event::KeyModifiers) -> Modifiers {
    let mut out = Modifiers::NONE;
    for (source, target) in [
        (event::KeyModifiers::SHIFT, Modifiers::SHIFT),
        (event::KeyModifiers::CONTROL, Modifiers::CONTROL),
        (event::KeyModifiers::ALT, Modifiers::ALT),
        (event::KeyModifiers::SUPER, Modifiers::SUPER),
        (event::KeyModifiers::HYPER, Modifiers::HYPER),
        (event::KeyModifiers::META, Modifiers::META),
    ] {
        if mods.contains(source) {
            out |= target;
        }
    }
    out
}

fn translate_mouse(mouse: event::MouseEvent) -> Option<MouseEvent> {
    let kind = match mouse.kind {
        event::MouseEventKind::Moved => MouseKind::Moved,
        event::MouseEventKind::Down(event::MouseButton::Left) => MouseKind::LeftDown,
        event::MouseEventKind::Drag(event::MouseButton::Left) => MouseKind::LeftDrag,
        event::MouseEventKind::Up(event::MouseButton::Left) => MouseKind::LeftUp,
        _ => return None,
    };
    Some(MouseEvent {
        column: mouse.column,
        row: mouse.row,
        kind,
    })
}

fn main() -> ExitCode {
    let matches = Cli::command().get_matches();
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(err) => {
            err.print().ok();
            return ExitCode::FAILURE;
        }
    };

    if let Some(output) = cli.capture.capture.clone() {
        let result = capture::config_from_args(&cli.capture, output, &matches, cli.files).and_then(capture::run);
        return match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("apus: {err}");
                ExitCode::FAILURE
            }
        };
    }

    match run_terminal(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("apus: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run_terminal(cli: &Cli) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture, EnableFocusChange)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run(&mut terminal, cli);

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableFocusChange,
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;

    result
}

fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, cli: &Cli) -> io::Result<()> {
    let mut app = build_app(&cli.files);

    let mut settle_deadline = Some(Instant::now() + DRAW_ACTIVITY_WINDOW);
    while !app.should_quit() {
        // Blocks when fully settled so an idle viewer costs nothing; polls while animating.
        let repeat_wait = app
            .mouse_repeat_deadline()
            .map(|deadline| deadline.saturating_duration_since(Instant::now()));
        let has_input = match (should_poll_rather_than_block(settle_deadline), repeat_wait) {
            (true, Some(wait)) => event::poll(ANIMATION_FRAME_INTERVAL.min(wait))?,
            (true, None) => event::poll(ANIMATION_FRAME_INTERVAL)?,
            (false, Some(wait)) => event::poll(wait)?,
            (false, None) => true,
        };

        // Drain every queued event before drawing. Consuming one per draw let the queue outlive the
        // keypress: motion visibly continued after release while the loop worked through a stale
        // backlog, and each intermediate position queued its own BAM fetch. Draining collapses a
        // burst into one draw at the final position, so only that position is fetched.
        let mut had_input_event = false;
        if has_input {
            let mut drained = 0usize;
            loop {
                match event::read()? {
                    // Windows reports a `Release` for every key as well as its `Press`; other
                    // platforms only do once keyboard enhancement is enabled. Acting on releases
                    // doubled every key (Tab cycled forward and straight back) and replayed the
                    // Enter that launched the binary as `JumpToPeer`. `Repeat` stays live so a
                    // held key keeps moving the view where the terminal reports repeats that way.
                    Event::Key(key) if key.kind == KeyEventKind::Release => {}
                    Event::Key(key) => {
                        if let Some(code) = translate_key(key.code) {
                            app.on_key(code, translate_modifiers(key.modifiers));
                        }
                        had_input_event = true;
                    }
                    Event::Mouse(mouse) => {
                        if let Some(mouse) = translate_mouse(mouse) {
                            app.on_mouse(mouse);
                        }
                        had_input_event = true;
                    }
                    Event::FocusLost => app.cancel_mouse_gesture(),
                    Event::Resize(_, _) => {
                        app.cancel_mouse_gesture();
                        had_input_event = true;
                    }
                    _ => {}
                }
                drained += 1;
                if drain_should_stop(app.should_quit(), drained, event::poll(Duration::ZERO)?) {
                    break;
                }
            }
        }

        let viewer_activity = app.tick();
        let real_draw_required = had_input_event || viewer_activity;

        let decision = advance_settle_deadline(settle_deadline, real_draw_required, Instant::now());
        settle_deadline = decision.next_deadline;
        if !decision.should_draw {
            continue;
        }

        terminal.draw(|frame| {
            let [tab_bar, content] = frame_layout(frame.area());
            app.draw(frame, tab_bar, content, decision.loop_state);
        })?;
    }

    Ok(())
}
