//! Snapper: a lightweight region screenshot tool that lives in the tray (Windows and Linux).
//!
//! Modes in one executable:
//! - resident (default): tray icon, global hotkey and the capture overlay;
//! - settings (`--settings`): an egui window that edits the config and signals the resident process;
//! - Linux only: `--overlay <dir>` (the capture overlay, started by the resident process) and
//!   `--capture` (ask the running instance for a screenshot, or start one and capture at once).

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod ann;
#[cfg(windows)]
mod capture;
#[cfg(unix)]
#[path = "capture_linux.rs"]
mod capture;
#[cfg(windows)]
mod clipboard;
#[cfg(unix)]
#[path = "clipboard_linux.rs"]
mod clipboard;
mod config;
mod files;
mod geom;
mod hotkey;
#[allow(dead_code)]
mod i18n;
#[allow(dead_code)]
mod nimvoro;
#[cfg(windows)]
mod overlay;
#[cfg(unix)]
#[path = "overlay_linux.rs"]
mod overlay;
#[cfg(unix)]
mod raster;
#[cfg(windows)]
mod render;
#[cfg(windows)]
mod resident;
#[cfg(unix)]
#[path = "resident_linux.rs"]
mod resident;
mod settings;
mod strings;
mod support;
#[cfg(windows)]
mod sys;
#[cfg(unix)]
#[path = "sys_linux.rs"]
mod sys;
#[allow(dead_code)]
mod theme;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--settings") {
        if settings::run().is_err() {
            std::process::exit(1);
        }
        return;
    }
    #[cfg(unix)]
    {
        if let Some(i) = args.iter().position(|a| a == "--overlay") {
            let dir = args.get(i + 1).map(std::path::PathBuf::from).unwrap_or_default();
            std::process::exit(overlay::run(&dir));
        }
        std::process::exit(resident::run(args.iter().any(|a| a == "--capture")));
    }
    #[cfg(windows)]
    std::process::exit(resident::run());
}
