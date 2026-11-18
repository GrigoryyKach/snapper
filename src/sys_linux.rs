//! Linux helpers shared by every mode: autostart (`~/.config/autostart/snapper.desktop`), the
//! resident-process status file, the control socket between the resident process, `--settings`
//! and `--capture`, and opening URLs and folders.

use crate::config;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

// ───────────────────────── executable path and .desktop files ─────────────────────────

/// The path to start Snapper with: the AppImage file when running from one, else the binary.
pub fn exe_path() -> Option<PathBuf> {
    if let Some(a) = std::env::var_os("APPIMAGE") {
        if !a.is_empty() {
            return Some(PathBuf::from(a));
        }
    }
    std::env::current_exe().ok()
}

/// Quotes one argument for the `Exec=` key of a desktop entry (Desktop Entry Specification, "Exec key").
pub fn desktop_exec_quote(arg: &str) -> String {
    let needs = arg.is_empty() || arg.chars().any(|c| " \t\n\"'\\><~|&;$*?#()`%".contains(c));
    if !needs {
        return arg.to_string();
    }
    let mut out = String::from("\"");
    for c in arg.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            out.push('\\');
        }
        if c == '%' {
            out.push('%'); // a literal percent is written %%
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// The shell-style command that takes a screenshot, for Wayland shortcut bindings.
pub fn capture_command() -> String {
    let exe = exe_path().map(|p| p.display().to_string()).unwrap_or_else(|| "Snapper".into());
    let q = if exe.contains(' ') { format!("\"{exe}\"") } else { exe };
    format!("{q} --capture")
}

pub fn autostart_desktop(exec_line: &str) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=Snapper\nComment=Region screenshots from the tray\nExec={exec_line}\nTerminal=false\nX-GNOME-Autostart-enabled=true\n"
    )
}

fn autostart_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("autostart").join("snapper.desktop"))
}

pub fn autostart_enabled() -> bool {
    autostart_path().is_some_and(|p| p.exists())
}

/// Adds or removes the autostart entry. Returns `false` when the file could not be written.
pub fn set_autostart(on: bool) -> bool {
    let Some(p) = autostart_path() else { return false };
    if !on {
        return match std::fs::remove_file(&p) {
            Ok(()) => true,
            Err(e) => e.kind() == std::io::ErrorKind::NotFound,
        };
    }
    let Some(exe) = exe_path() else { return false };
    let line = format!("{} --minimized", desktop_exec_quote(&exe.display().to_string()));
    if let Some(d) = p.parent() {
        if std::fs::create_dir_all(d).is_err() {
            return false;
        }
    }
    std::fs::write(&p, autostart_desktop(&line)).is_ok()
}

/// Only Windows has a Snipping Tool that steals Print Screen.
pub fn prtsc_snipping_enabled() -> bool {
    false
}

// ───────────────────────── resident <-> settings / --capture ─────────────────────────

pub fn socket_path() -> PathBuf {
    if std::env::var_os("SNAPPER_DATA_DIR").is_none() {
        if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") {
            return PathBuf::from(rt).join("snapper.sock");
        }
    }
    config::data_dir().join("snapper.sock")
}

/// Sends one command (`capture`, `settings`, `reload`, `quit`) to the resident process.
/// Returns `false` when nobody listens.
pub fn send(cmd: &str) -> bool {
    let Ok(mut s) = UnixStream::connect(socket_path()) else { return false };
    let _ = s.set_write_timeout(Some(Duration::from_millis(500)));
    s.write_all(format!("{cmd}\n").as_bytes()).is_ok()
}

pub fn signal_reload() -> bool {
    send("reload")
}

/// `Some(())` while a resident process answers on the control socket.
pub fn resident_window() -> Option<()> {
    UnixStream::connect(socket_path()).ok().map(|_| ())
}

/// Binds the control socket; `None` when another instance already owns it.
pub fn listen() -> Option<UnixListener> {
    let p = socket_path();
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if UnixStream::connect(&p).is_ok() {
        return None;
    }
    let _ = std::fs::remove_file(&p); // stale socket from a crashed run
    UnixListener::bind(&p).ok()
}

/// Reads the commands of one connection.
pub fn read_commands(s: UnixStream) -> Vec<String> {
    let _ = s.set_read_timeout(Some(Duration::from_millis(500)));
    let mut buf = String::new();
    let _ = s.take(256).read_to_string(&mut buf);
    buf.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect()
}

/// Written by the resident process after each hotkey registration attempt.
pub fn status_path() -> PathBuf {
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

fn spawn_quiet(prog: &str, args: &[&std::ffi::OsStr]) {
    let _ = Command::new(prog).args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
}

pub fn open_url(url: &str) {
    spawn_quiet("xdg-open", &[url.as_ref()]);
}

pub fn open_folder(dir: &Path) {
    let _ = std::fs::create_dir_all(dir);
    spawn_quiet("xdg-open", &[dir.as_os_str()]);
}

/// Opens the folder holding the file (there is no portable "select this file" on Linux).
pub fn reveal_file(path: &Path) {
    if let Some(d) = path.parent() {
        spawn_quiet("xdg-open", &[d.as_os_str()]);
    }
}

/// True under a Wayland session (compositor-owned screen, no global hotkeys, portal capture).
pub fn is_wayland() -> bool {
    session_is_wayland(std::env::var("XDG_SESSION_TYPE").ok().as_deref(), std::env::var_os("WAYLAND_DISPLAY").is_some())
}

pub fn session_is_wayland(session_type: Option<&str>, wayland_display: bool) -> bool {
    match session_type {
        Some(t) if t.eq_ignore_ascii_case("wayland") => true,
        Some(t) if t.eq_ignore_ascii_case("x11") => false,
        _ => wayland_display,
    }
}

/// Diagnostics: appends a line to `snapper.log` in the data folder when `SNAPPER_LOG` is set.
pub fn log(msg: &str) {
    if std::env::var_os("SNAPPER_LOG").is_none() {
        return;
    }
    let _ = std::fs::create_dir_all(config::data_dir());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(config::data_dir().join("snapper.log")) {
        let _ = writeln!(f, "{msg}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_quoting() {
        assert_eq!(desktop_exec_quote("/opt/snapper/Snapper"), "/opt/snapper/Snapper");
        assert_eq!(desktop_exec_quote("/home/a b/Snapper"), "\"/home/a b/Snapper\"");
        assert_eq!(desktop_exec_quote("/x/a$b"), "\"/x/a\\$b\"");
        assert_eq!(desktop_exec_quote("/x/50%"), "\"/x/50%%\"");
        assert_eq!(desktop_exec_quote("/x/5 0%"), "\"/x/5 0%%\"");
    }

    #[test]
    fn autostart_entry_is_valid() {
        let d = autostart_desktop("\"/home/a b/Snapper\" --minimized");
        assert!(d.starts_with("[Desktop Entry]\n"));
        assert!(d.contains("\nExec=\"/home/a b/Snapper\" --minimized\n"));
        assert!(d.contains("Type=Application"));
    }

    #[test]
    fn session_detection() {
        assert!(session_is_wayland(Some("wayland"), false));
        assert!(!session_is_wayland(Some("x11"), true));
        assert!(session_is_wayland(None, true));
        assert!(!session_is_wayland(None, false));
        assert!(!session_is_wayland(Some("tty"), false));
    }

    #[test]
    fn control_socket_roundtrip() {
        let dir = std::env::temp_dir().join(format!("snapper-sock-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let sock = dir.join("t.sock");
        let l = UnixListener::bind(&sock).unwrap();
        let mut c = UnixStream::connect(&sock).unwrap();
        c.write_all(b"capture\nreload\n").unwrap();
        drop(c);
        let (s, _) = l.accept().unwrap();
        assert_eq!(read_commands(s), vec!["capture".to_string(), "reload".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
