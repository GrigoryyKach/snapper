//! Snapper: a lightweight region screenshot tool that lives in the tray (Windows only).
//!
//! Two modes in one exe:
//! - resident (default): hidden Win32 window, tray icon, global hotkey and the capture overlay;
//! - settings (`--settings`): an egui window that edits `%APPDATA%\Snapper\config.json`
//!   and signals the resident process.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod capture;
mod clipboard;
mod config;
mod files;
mod geom;
mod hotkey;
#[allow(dead_code)]
mod i18n;
#[allow(dead_code)]
mod nimvoro;
mod overlay;
mod render;
mod resident;
mod settings;
mod strings;
mod support;
mod sys;
#[allow(dead_code)]
mod theme;

fn main() {
    let settings_mode = std::env::args().skip(1).any(|a| a == "--settings");
    if settings_mode {
        if settings::run().is_err() {
            std::process::exit(1);
        }
    } else {
        std::process::exit(resident::run());
    }
}
