//! Topebag executable: the desktop app, plus a small CLI.
//!
//! Run without arguments to open the desktop app; run with a subcommand
//! (`topebag add <url>`, `topebag list`, …) for the command line. See
//! `topebag --help`.

// Release builds are GUI-subsystem apps (no console window flashes up when
// launched from Explorer). The CLI re-attaches to the parent console.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod cli;
mod gui;

use std::process::ExitCode;

fn main() -> ExitCode {
    let has_args = std::env::args_os().len() > 1;
    if has_args {
        attach_parent_console();
    }
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    match cli::parse() {
        cli::Mode::Gui { urls, background } => match gui::run(urls, background) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("Topebag could not start: {err:#}");
                ExitCode::FAILURE
            }
        },
        cli::Mode::Command(command) => cli::run(command),
    }
}

/// In release builds the binary uses the Windows GUI subsystem, so it has
/// no console of its own. When started from a terminal with arguments,
/// attach to that terminal so CLI output is visible.
fn attach_parent_console() {
    #[cfg(all(windows, not(debug_assertions)))]
    {
        use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
        // SAFETY: plain Win32 call; failure (no parent console) is harmless.
        unsafe {
            let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}
