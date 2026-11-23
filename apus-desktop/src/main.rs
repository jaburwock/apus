//! Native `wgpu` desktop window and headless PNG capture runner for Apus.
//!
//! `--capture <PATH>` is parsed before creating any `winit` event loop or window so headless
//! captures run under Mesa `llvmpipe` with `DISPLAY` and `WAYLAND_DISPLAY` unset.

mod headless;
mod window;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use apus::FileArgs;
use clap::{CommandFactory, FromArgMatches, Parser};

use headless::{CaptureConfig, ScriptStep, parse_key_spec};
use window::WindowConfig;

const DEFAULT_WINDOW_WIDTH: u32 = 1280;
const DEFAULT_WINDOW_HEIGHT: u32 = 720;
const DEFAULT_WINDOW_FONT_SIZE: f32 = 15.0;

#[derive(Parser, Debug)]
#[command(
    name = "apus-desktop",
    about = "Native GPU desktop and headless capture runner for Apus"
)]
struct DesktopCli {
    /// Render offscreen under Mesa llvmpipe, write one PNG to this path, and exit.
    #[arg(long, value_name = "PATH")]
    capture: Option<PathBuf>,

    #[arg(long, hide = true)]
    crt: bool,

    /// Framebuffer / window width in pixels (logical pixels in windowed mode).
    /// Defaults to 140 cells wide in `--capture` mode and 1280 logical pixels in windowed mode.
    #[arg(long)]
    width: Option<u32>,

    /// Framebuffer / window height in pixels (logical pixels in windowed mode).
    /// Defaults to 40 cells high in `--capture` mode and 720 logical pixels in windowed mode.
    #[arg(long)]
    height: Option<u32>,

    /// Font em size in logical points.
    /// Defaults to 12pt in `--capture` mode (`980x600` for 140x40 cells at 1x) and 15pt in windowed mode.
    #[arg(long)]
    font_size: Option<f32>,

    /// DPI scale factor for headless capture.
    #[arg(long, default_value_t = 1.0)]
    scale: f32,

    /// Timeout in milliseconds for headless capture settling.
    #[arg(long, default_value_t = 10_000)]
    timeout_ms: u64,

    /// Apply Slug optical weight boost (`sqrt(coverage)`) for thicker 1x DPI strokes.
    #[arg(long, default_value_t = false)]
    weight_boost: bool,

    /// Scripted command to execute in `--capture` mode (repeatable; preserves order with `--key`).
    #[arg(long = "cmd", value_name = "COMMAND")]
    cmds: Vec<String>,

    /// Scripted key event to send in `--capture` mode (repeatable; preserves order with `--cmd`).
    #[arg(long = "key", value_name = "KEY")]
    keys: Vec<String>,

    #[command(flatten)]
    files: FileArgs,
}

fn collect_ordered_steps(matches: &clap::ArgMatches) -> Result<Vec<ScriptStep>, String> {
    let mut indexed: Vec<(usize, ScriptStep)> = Vec::new();
    if let (Some(indices), Some(values)) = (matches.indices_of("cmds"), matches.get_many::<String>("cmds")) {
        for (idx, cmd) in indices.zip(values) {
            indexed.push((idx, ScriptStep::Command(cmd.clone())));
        }
    }
    if let (Some(indices), Some(values)) = (matches.indices_of("keys"), matches.get_many::<String>("keys")) {
        for (idx, spec) in indices.zip(values) {
            let (key, mods) = parse_key_spec(spec)?;
            indexed.push((idx, ScriptStep::Key(key, mods)));
        }
    }
    indexed.sort_by_key(|(idx, _)| *idx);
    Ok(indexed.into_iter().map(|(_, step)| step).collect())
}

fn main() -> ExitCode {
    let matches = DesktopCli::command().get_matches();
    let cli = match DesktopCli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(err) => {
            err.print().ok();
            return ExitCode::FAILURE;
        }
    };

    let result = if let Some(output) = cli.capture {
        match collect_ordered_steps(&matches) {
            Ok(steps) => headless::run(CaptureConfig {
                output,
                width: cli.width,
                height: cli.height,
                font_size: cli.font_size.unwrap_or(headless::DEFAULT_CAPTURE_FONT_SIZE),
                scale: cli.scale,
                timeout: Duration::from_millis(cli.timeout_ms),
                weight_boost: cli.weight_boost,
                crt: cli.crt,
                steps,
                files: cli.files,
            }),
            Err(err) => Err(err),
        }
    } else {
        window::run(WindowConfig {
            width: cli.width.unwrap_or(DEFAULT_WINDOW_WIDTH),
            height: cli.height.unwrap_or(DEFAULT_WINDOW_HEIGHT),
            font_size: cli.font_size.unwrap_or(DEFAULT_WINDOW_FONT_SIZE),
            weight_boost: cli.weight_boost,
            crt: cli.crt,
            files: cli.files,
        })
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("apus-desktop: {err}");
            ExitCode::FAILURE
        }
    }
}
