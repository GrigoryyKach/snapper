//! Hotkey model: parsing and formatting strings like `Ctrl+Shift+S` or `PrintScreen`.
//! Modifier bits match the Win32 `MOD_*` constants, so they go straight into `RegisterHotKey`.

pub const MOD_ALT: u32 = 0x1;
pub const MOD_CONTROL: u32 = 0x2;
pub const MOD_SHIFT: u32 = 0x4;
pub const MOD_WIN: u32 = 0x8;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Hotkey {
    pub mods: u32,
    pub vk: u32,
}

pub const VK_SNAPSHOT: u32 = 0x2C;

impl Default for Hotkey {
    fn default() -> Self {
        Hotkey { mods: 0, vk: VK_SNAPSHOT }
    }
}

const NAMED: &[(&str, u32)] = &[
    ("PrintScreen", 0x2C),
    ("Pause", 0x13),
    ("ScrollLock", 0x91),
    ("Insert", 0x2D),
    ("Delete", 0x2E),
    ("Home", 0x24),
    ("End", 0x23),
    ("PageUp", 0x21),
    ("PageDown", 0x22),
    ("Space", 0x20),
    ("Enter", 0x0D),
    ("Tab", 0x09),
    ("Backspace", 0x08),
    ("Left", 0x25),
    ("Up", 0x26),
    ("Right", 0x27),
    ("Down", 0x28),
    ("Tilde", 0xC0),
];

/// Display name of a virtual-key code, `None` if the key is not supported as a hotkey.
pub fn key_name(vk: u32) -> Option<String> {
    if (0x41..=0x5A).contains(&vk) || (0x30..=0x39).contains(&vk) {
        return Some((vk as u8 as char).to_string());
    }
    if (0x70..=0x87).contains(&vk) {
        return Some(format!("F{}", vk - 0x6F));
    }
    NAMED.iter().find(|(_, v)| *v == vk).map(|(n, _)| n.to_string())
}

pub fn key_from_name(name: &str) -> Option<u32> {
    let n = name.trim();
    if n.chars().count() == 1 {
        let c = n.chars().next()?.to_ascii_uppercase();
        if c.is_ascii_uppercase() || c.is_ascii_digit() {
            return Some(c as u32);
        }
    }
    if let Some(num) = n.strip_prefix(['F', 'f']) {
        if let Ok(k) = num.parse::<u32>() {
            if (1..=24).contains(&k) {
                return Some(0x6F + k);
            }
        }
    }
    NAMED.iter().find(|(nm, _)| nm.eq_ignore_ascii_case(n)).map(|(_, v)| *v)
}

/// Keys that may be a hotkey without any modifier (function-like keys).
pub fn allowed_without_modifier(vk: u32) -> bool {
    (0x70..=0x87).contains(&vk) || matches!(vk, 0x2C | 0x13 | 0x91 | 0x2D)
}

impl Hotkey {
    pub fn parse(s: &str) -> Option<Hotkey> {
        let mut mods = 0;
        let mut vk = None;
        for part in s.split('+') {
            let p = part.trim();
            if p.is_empty() {
                return None;
            }
            match p.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => mods |= MOD_CONTROL,
                "alt" => mods |= MOD_ALT,
                "shift" => mods |= MOD_SHIFT,
                "win" | "windows" | "super" => mods |= MOD_WIN,
                _ => {
                    if vk.is_some() {
                        return None;
                    }
                    vk = Some(key_from_name(p)?);
                }
            }
        }
        let vk = vk?;
        if mods == 0 && !allowed_without_modifier(vk) {
            return None;
        }
        Some(Hotkey { mods, vk })
    }

    pub fn format(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.mods & MOD_CONTROL != 0 {
            parts.push("Ctrl".into());
        }
        if self.mods & MOD_ALT != 0 {
            parts.push("Alt".into());
        }
        if self.mods & MOD_SHIFT != 0 {
            parts.push("Shift".into());
        }
        if self.mods & MOD_WIN != 0 {
            parts.push("Win".into());
        }
        parts.push(key_name(self.vk).unwrap_or_else(|| format!("0x{:02X}", self.vk)));
        parts.join("+")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_formats() {
        let h = Hotkey::parse("ctrl+shift+s").unwrap();
        assert_eq!(h, Hotkey { mods: MOD_CONTROL | MOD_SHIFT, vk: 0x53 });
        assert_eq!(h.format(), "Ctrl+Shift+S");
        assert_eq!(Hotkey::parse("PrintScreen"), Some(Hotkey { mods: 0, vk: VK_SNAPSHOT }));
        assert_eq!(Hotkey::parse("Win+Alt+F10").unwrap().format(), "Alt+Win+F10");
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!(Hotkey::parse(""), None);
        assert_eq!(Hotkey::parse("Ctrl+"), None);
        assert_eq!(Hotkey::parse("Ctrl+Shift"), None);
        assert_eq!(Hotkey::parse("S"), None); // a letter alone would break typing
        assert_eq!(Hotkey::parse("Ctrl+A+B"), None);
        assert_eq!(Hotkey::parse("Ctrl+Banana"), None);
    }

    #[test]
    fn roundtrip() {
        for s in ["PrintScreen", "Ctrl+Alt+Z", "Shift+F12", "Ctrl+Win+Space", "F9", "Ctrl+5"] {
            assert_eq!(Hotkey::parse(s).unwrap().format(), s);
        }
    }
}
