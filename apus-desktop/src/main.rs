//! Native `wgpu` desktop window and headless PNG capture runner for Apus.
//!
//! `--capture <PATH>` runs the shared CPU capture runner before any `winit` or `wgpu` setup so it
//! requires no display server, graphics driver, or Mesa.
//! `--llvmcapture <PATH>` runs offscreen under Mesa `llvmpipe` (Slug + optional `--crt`) without
//! creating a window so native renderer tests run with `DISPLAY` and `WAYLAND_DISPLAY` unset.

mod headless;
mod window;

use std::path::PathBuf;
use std::process::ExitCode;

use apus::capture::{self, config_from_args};
use apus::{CaptureArgs, FileArgs};
use clap::{CommandFactory, FromArgMatches, Parser};

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
    /// Render offscreen under Mesa llvmpipe (Slug + optional `--crt`), write one PNG to this path, and exit.
    #[arg(long, value_name = "PATH", conflicts_with = "capture")]
    llvmcapture: Option<PathBuf>,

    #[arg(long, hide = true, conflicts_with = "capture")]
    crt: bool,

    #[command(flatten)]
    capture: CaptureArgs,

    #[command(flatten)]
    files: FileArgs,
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

    let result = if let Some(output) = cli.capture.capture.clone() {
        config_from_args(&cli.capture, output, &matches, cli.files).and_then(capture::run)
    } else if let Some(output) = cli.llvmcapture {
        config_from_args(&cli.capture, output, &matches, cli.files).and_then(|cfg| headless::run(cfg, cli.crt))
    } else {
        window::run(WindowConfig {
            width: cli.capture.width.unwrap_or(DEFAULT_WINDOW_WIDTH),
            height: cli.capture.height.unwrap_or(DEFAULT_WINDOW_HEIGHT),
            font_size: cli.capture.font_size.unwrap_or(DEFAULT_WINDOW_FONT_SIZE),
            weight_boost: cli.capture.weight_boost,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_cli_rejects_capture_combined_with_llvmcapture_or_crt() {
        assert!(DesktopCli::try_parse_from(["apus-desktop", "--capture", "a.png", "--llvmcapture", "b.png"]).is_err());
        assert!(DesktopCli::try_parse_from(["apus-desktop", "--capture", "a.png", "--crt"]).is_err());
        assert!(DesktopCli::try_parse_from(["apus-desktop", "--llvmcapture", "a.png", "--crt"]).is_ok());
        assert!(DesktopCli::try_parse_from(["apus-desktop", "--crt"]).is_ok());
        assert!(DesktopCli::try_parse_from(["apus-desktop", "--capture", "a.png"]).is_ok());
    }
}
