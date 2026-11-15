//! Small Win32 helpers shared by both modes: autostart (HKCU Run), the Print Screen / Snipping Tool
//! flag, resident-process status file and signalling, URL/folder opening.

use crate::config;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{ERROR_SUCCESS, HWND, LPARAM, WPARAM};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
    KEY_QUERY_VALUE, KEY_SET_VALUE, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ, REG_VALUE_TYPE,
};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW, RegisterWindowMessageW};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
pub const RESIDENT_CLASS: PCWSTR = w!("SnapperResident");
const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("Snapper");

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `"C:\path\Snapper.exe"` with quotes, exactly as stored in the Run key.
pub fn quoted_exe() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    Some(format!("\"{}\"", exe.display()))
}

pub fn autostart_enabled() -> bool {
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, None, KEY_QUERY_VALUE, &mut key) != ERROR_SUCCESS {
            return false;
        }
        let mut ty = REG_VALUE_TYPE(0);
        let mut size = 0u32;
        let r = RegQueryValueExW(key, RUN_VALUE, None, Some(&mut ty), None, Some(&mut size));
        let _ = RegCloseKey(key);
        r == ERROR_SUCCESS
    }
}

/// Adds or removes the Run value. Returns `false` when the registry refused.
pub fn set_autostart(on: bool) -> bool {
    unsafe {
        let mut key = HKEY::default();
        if RegCreateKeyExW(HKEY_CURRENT_USER, RUN_KEY, None, PCWSTR::null(), REG_OPTION_NON_VOLATILE, KEY_SET_VALUE, None, &mut key, None)
            != ERROR_SUCCESS
        {
            return false;
        }
        let ok = if on {
            let Some(path) = quoted_exe() else {
                let _ = RegCloseKey(key);
                return false;
            };
            let data: Vec<u8> = wide(&path).iter().flat_map(|c| c.to_le_bytes()).collect();
            RegSetValueExW(key, RUN_VALUE, None, REG_SZ, Some(&data)) == ERROR_SUCCESS
        } else {
            let r = RegDeleteValueW(key, RUN_VALUE);
            r == ERROR_SUCCESS || r.0 == 2 // 2 = value did not exist
        };
        let _ = RegCloseKey(key);
        ok
    }
}

/// True when Windows hands Print Screen to the Snipping Tool (Accessibility > Keyboard).
pub fn prtsc_snipping_enabled() -> bool {
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, w!("Control Panel\\Keyboard"), None, KEY_QUERY_VALUE, &mut key) != ERROR_SUCCESS {
            return false;
        }
        let mut ty = REG_VALUE_TYPE(0);
        let mut data = [0u8; 4];
        let mut size = 4u32;
        let r = RegQueryValueExW(key, w!("PrintScreenKeyForSnippingEnabled"), None, Some(&mut ty), Some(data.as_mut_ptr()), Some(&mut size));
        let _ = RegCloseKey(key);
        r == ERROR_SUCCESS && ty == REG_DWORD && u32::from_le_bytes(data) != 0
    }
}

// ───────────────────────── resident <-> settings ─────────────────────────

pub fn msg_reload() -> u32 {
    unsafe { RegisterWindowMessageW(w!("Snapper.ReloadConfig")) }
}

pub fn msg_show_settings() -> u32 {
    unsafe { RegisterWindowMessageW(w!("Snapper.ShowSettings")) }
}

pub fn resident_window() -> Option<HWND> {
    unsafe { FindWindowW(RESIDENT_CLASS, PCWSTR::null()).ok() }
}

/// Tells the resident process to reload the config. Returns `false` if it is not running.
pub fn signal_reload() -> bool {
    match resident_window() {
        Some(h) => unsafe { PostMessageW(Some(h), msg_reload(), WPARAM(0), LPARAM(0)).is_ok() },
        None => false,
    }
}

/// Written by the resident process after each hotkey registration attempt.
pub fn status_path() -> std::path::PathBuf {
    config::data_dir().join("status.json")
}

#[derive(Clone, PartialEq, Debug)]
pub struct HotkeyStatus {
    pub hotkey: String,
    pub ok: bool,
}

pub fn write_status(hotkey: &str, ok: bool) {
    let v = serde_json::json!({ "hotkey": hotkey, "ok": ok });
    let _ = config::write_atomic(&status_path(), v.to_string().as_bytes());
}

pub fn read_status() -> Option<HotkeyStatus> {
    let text = std::fs::read_to_string(status_path()).ok()?;
    let v: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()?;
    Some(HotkeyStatus { hotkey: v.get("hotkey")?.as_str()?.to_string(), ok: v.get("ok")?.as_bool()? })
}

// ───────────────────────── opening things ─────────────────────────

pub fn open_url(url: &str) {
    let _ = Command::new("rundll32.exe").args(["url.dll,FileProtocolHandler", url]).creation_flags(CREATE_NO_WINDOW).spawn();
}

pub fn open_folder(dir: &Path) {
    let _ = std::fs::create_dir_all(dir);
    let _ = Command::new("explorer.exe").arg(dir).spawn();
}

/// Opens Explorer with the file selected.
pub fn reveal_file(path: &Path) {
    let _ = Command::new("explorer.exe").arg(format!("/select,{}", path.display())).spawn();
}

/// Diagnostics: appends a line to `%APPDATA%\Snapper\snapper.log` when `SNAPPER_LOG` is set.
pub fn log(msg: &str) {
    if std::env::var_os("SNAPPER_LOG").is_none() {
        return;
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(config::data_dir().join("snapper.log")) {
        let _ = writeln!(f, "{msg}");
    }
}
