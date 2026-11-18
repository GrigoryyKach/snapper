//! Snapper configuration: `%APPDATA%\Snapper\config.json` (override the folder with `SNAPPER_DATA_DIR`).

use crate::theme::ThemeMode;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    #[default]
    Png,
    Jpg,
}

impl Format {
    pub fn ext(self) -> &'static str {
        match self {
            Format::Png => "png",
            Format::Jpg => "jpg",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Config {
    /// Trigger hotkey, e.g. `PrintScreen` or `Ctrl+Shift+S`.
    pub hotkey: String,
    pub autostart: bool,
    /// Folder for saved screenshots; empty means `%USERPROFILE%\Pictures\Snapper`.
    pub save_dir: String,
    pub format: Format,
    pub copy_on_save: bool,
    /// Interface language code (`en`, `ru`, `uk`); `None` until first resolved.
    pub language: Option<String>,
    pub theme: ThemeMode,
    pub check_updates: bool,
    /// Last drawing colour as `#rrggbb`.
    pub last_color: String,
    /// Last stroke width step: 0 thin, 1 medium, 2 thick.
    pub last_stroke: u8,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hotkey: "PrintScreen".into(),
            autostart: false,
            save_dir: String::new(),
            format: Format::Png,
            copy_on_save: false,
            language: None,
            theme: ThemeMode::System,
            check_updates: true,
            last_color: "#ff3b30".into(),
            last_stroke: 1,
        }
    }
}

/// `%APPDATA%\Snapper` (or `SNAPPER_DATA_DIR`).
pub fn data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("SNAPPER_DATA_DIR") {
        return PathBuf::from(d);
    }
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    // $XDG_CONFIG_HOME or ~/.config
    #[cfg(unix)]
    let base = dirs::config_dir().unwrap_or_else(std::env::temp_dir);
    base.join("Snapper")
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

/// Parses config JSON (tolerates a UTF-8 BOM and unknown/missing keys).
pub fn parse(text: &str) -> Option<Config> {
    serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()
}

/// Writes `data` to `path` through a temporary file and a rename, so a crash never leaves half a file.
pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

impl Config {
    pub fn load() -> Self {
        Self::load_from(&config_path())
    }

    pub fn load_from(path: &Path) -> Self {
        let mut cfg = std::fs::read_to_string(path).ok().and_then(|t| parse(&t)).unwrap_or_default();
        cfg.last_stroke = cfg.last_stroke.min(2);
        cfg
    }

    pub fn save(&self) {
        let _ = self.save_to(&config_path());
    }

    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        write_atomic(path, json.as_bytes())
    }

    /// Effective save folder.
    pub fn save_folder(&self) -> PathBuf {
        if !self.save_dir.trim().is_empty() {
            return PathBuf::from(self.save_dir.trim());
        }
        default_save_dir()
    }
}

#[cfg(windows)]
pub fn default_save_dir() -> PathBuf {
    std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("Pictures").join("Snapper")
}

/// `~/Pictures/Snapper`, following the XDG pictures folder when the user has one.
#[cfg(unix)]
pub fn default_save_dir() -> PathBuf {
    dirs::picture_dir().or_else(|| dirs::home_dir().map(|h| h.join("Pictures"))).unwrap_or_else(std::env::temp_dir).join("Snapper")
}

/// Parses `#rrggbb` (or `rrggbb`) into `0xRRGGBB`.
pub fn parse_hex_color(s: &str) -> Option<u32> {
    let t = s.trim().trim_start_matches('#');
    if t.len() != 6 || !t.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(t, 16).ok()
}

pub fn format_hex_color(rgb: u32) -> String {
    format!("#{:06x}", rgb & 0xFF_FFFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_partial_json() {
        let c = parse("{\"hotkey\":\"Ctrl+Shift+S\",\"format\":\"jpg\"}").unwrap();
        assert_eq!(c.hotkey, "Ctrl+Shift+S");
        assert_eq!(c.format, Format::Jpg);
        assert!(c.check_updates);
        assert_eq!(c.last_color, "#ff3b30");
    }

    #[test]
    fn tolerates_bom() {
        let c = parse("\u{feff}{\"autostart\":true}").unwrap();
        assert!(c.autostart);
    }

    #[test]
    fn roundtrip_atomic() {
        let dir = std::env::temp_dir().join(format!("snapper-test-{}", std::process::id()));
        let path = dir.join("config.json");
        let mut c = Config::default();
        c.language = Some("uk".into());
        c.copy_on_save = true;
        c.save_to(&path).unwrap();
        assert!(!path.with_extension("json.tmp").exists());
        assert_eq!(Config::load_from(&path), c);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn broken_file_gives_defaults() {
        let dir = std::env::temp_dir().join(format!("snapper-test-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(Config::load_from(&path), Config::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hex_colors() {
        assert_eq!(parse_hex_color("#FF8000"), Some(0xff8000));
        assert_eq!(parse_hex_color("00ff00"), Some(0x00ff00));
        assert_eq!(parse_hex_color("#fff"), None);
        assert_eq!(parse_hex_color("#gg0000"), None);
        assert_eq!(format_hex_color(0x0a84ff), "#0a84ff");
    }

    #[test]
    fn default_folder_is_pictures_snapper() {
        let c = Config::default();
        assert!(c.save_folder().ends_with("Snapper"));
        let mut c2 = c.clone();
        c2.save_dir = "D:\\Shots".into();
        assert_eq!(c2.save_folder(), PathBuf::from("D:\\Shots"));
    }
}
