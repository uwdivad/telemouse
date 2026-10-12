//! A global hotkey chord as written in `telemouse.toml` (`ctrl+alt+r`),
//! parsed into what `RegisterHotKey` wants: modifier bits and a virtual-key
//! code. Pure, so the config can reject a typo at load time on any OS and
//! the tests do not need a window.
//!
//! Grammar: parts separated by `+`, case-insensitive, whitespace ignored.
//! Modifiers are `ctrl` (or `control`), `alt`, `shift`, `win` (or `super`,
//! `meta`); the last part is the key: a letter, a digit, `f1`–`f24`, or one
//! of the names in [`NAMED_KEYS`]. A letter, digit or punctuation key needs
//! at least one modifier — a bare `r` would steal every `r` typed on the
//! machine — while function keys and the like may stand alone. The empty
//! string, `none` and `off` mean no hotkey.

use std::fmt;

/// `MOD_ALT` etc. from winuser.h, as plain bits so this stays portable.
pub const MOD_ALT: u32 = 0x0001;
pub const MOD_CONTROL: u32 = 0x0002;
pub const MOD_SHIFT: u32 = 0x0004;
pub const MOD_WIN: u32 = 0x0008;

/// `VK_PAUSE`, `VK_SCROLL`, `VK_SNAPSHOT`, `VK_NUMLOCK`: the keys allowed
/// without a modifier (see [`STANDALONE`]).
const VK_PAUSE: u16 = 0x13;
const VK_SCROLL: u16 = 0x91;
const VK_SNAPSHOT: u16 = 0x2C;
const VK_NUMLOCK: u16 = 0x90;
/// `VK_F1` and `VK_F24`: the function-key range.
const VK_F1: u16 = 0x70;
const VK_F24: u16 = 0x87;

/// Named keys and their virtual-key codes (winuser.h `VK_*`).
pub const NAMED_KEYS: &[(&str, u16)] = &[
    ("space", 0x20),
    ("tab", 0x09),
    ("enter", 0x0D),
    ("return", 0x0D),
    ("esc", 0x1B),
    ("escape", 0x1B),
    ("backspace", 0x08),
    ("insert", 0x2D),
    ("delete", 0x2E),
    ("home", 0x24),
    ("end", 0x23),
    ("pageup", 0x21),
    ("pagedown", 0x22),
    ("up", 0x26),
    ("down", 0x28),
    ("left", 0x25),
    ("right", 0x27),
    ("pause", VK_PAUSE),
    ("scrolllock", VK_SCROLL),
    ("printscreen", VK_SNAPSHOT),
    ("numlock", VK_NUMLOCK),
    ("numpad0", 0x60),
    ("numpad1", 0x61),
    ("numpad2", 0x62),
    ("numpad3", 0x63),
    ("numpad4", 0x64),
    ("numpad5", 0x65),
    ("numpad6", 0x66),
    ("numpad7", 0x67),
    ("numpad8", 0x68),
    ("numpad9", 0x69),
    ("multiply", 0x6A),
    ("add", 0x6B),
    ("subtract", 0x6D),
    ("decimal", 0x6E),
    ("divide", 0x6F),
    ("-", 0xBD),
    ("=", 0xBB),
    ("[", 0xDB),
    ("]", 0xDD),
    (";", 0xBA),
    ("'", 0xDE),
    (",", 0xBC),
    (".", 0xBE),
    ("/", 0xBF),
    ("`", 0xC0),
    ("\\", 0xDC),
];

/// Keys that may be a hotkey on their own, without a modifier: nothing
/// types them, so grabbing them steals no text.
const STANDALONE: &[u16] = &[VK_PAUSE, VK_SCROLL, VK_SNAPSHOT, VK_NUMLOCK];

/// Why a chord in `telemouse.toml` cannot be registered. Every message
/// starts with the text as written, quoted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HotkeyError {
    /// Two `+` in a row, or a leading `+`.
    #[error("{text:?}: empty part (two '+' in a row?)")]
    EmptyPart { text: String },
    /// A part before the last that is not a modifier.
    #[error("{text:?}: {part:?} is not a modifier (ctrl, alt, shift, win); the key must come last")]
    NotAModifier { text: String, part: String },
    /// The last part names no key.
    #[error(
        "{text:?}: unknown key {key:?} (a letter, a digit, f1-f24, or one of: {})",
        named_key_list()
    )]
    UnknownKey { text: String, key: String },
    /// A typing key with no modifier.
    #[error(
        "{text:?}: a plain key would be taken away from every program; add ctrl, alt, shift or win"
    )]
    PlainKey { text: String },
}

/// Every name in [`NAMED_KEYS`], space-separated, for the error message.
fn named_key_list() -> String {
    NAMED_KEYS
        .iter()
        .map(|(n, _)| *n)
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
    /// Virtual-key code.
    pub vk: u16,
}

impl Hotkey {
    /// `Ok(None)` for "no hotkey"; `Err` names what is wrong with the text.
    pub fn parse(text: &str) -> Result<Option<Hotkey>, HotkeyError> {
        let t = text.trim();
        if t.is_empty() || t.eq_ignore_ascii_case("none") || t.eq_ignore_ascii_case("off") {
            return Ok(None);
        }
        let mut hk = Hotkey {
            ctrl: false,
            alt: false,
            shift: false,
            win: false,
            vk: 0,
        };
        let parts: Vec<String> = t
            .split('+')
            .map(|p| p.trim().to_ascii_lowercase())
            .collect();
        let (key, mods) = parts.split_last().expect("split yields at least one part");
        for m in mods {
            match m.as_str() {
                "ctrl" | "control" => hk.ctrl = true,
                "alt" => hk.alt = true,
                "shift" => hk.shift = true,
                "win" | "super" | "meta" => hk.win = true,
                "" => {
                    return Err(HotkeyError::EmptyPart {
                        text: text.to_string(),
                    });
                }
                other => {
                    return Err(HotkeyError::NotAModifier {
                        text: text.to_string(),
                        part: other.to_string(),
                    });
                }
            }
        }
        hk.vk = key_code(key).ok_or_else(|| HotkeyError::UnknownKey {
            text: text.to_string(),
            key: key.clone(),
        })?;
        if hk.modifiers() == 0 && !STANDALONE.contains(&hk.vk) && !is_function_key(hk.vk) {
            return Err(HotkeyError::PlainKey {
                text: text.to_string(),
            });
        }
        Ok(Some(hk))
    }

    /// The `fsModifiers` bits for `RegisterHotKey` (without `MOD_NOREPEAT`).
    pub fn modifiers(&self) -> u32 {
        let mut m = 0;
        if self.ctrl {
            m |= MOD_CONTROL;
        }
        if self.alt {
            m |= MOD_ALT;
        }
        if self.shift {
            m |= MOD_SHIFT;
        }
        if self.win {
            m |= MOD_WIN;
        }
        m
    }
}

fn is_function_key(vk: u16) -> bool {
    (VK_F1..=VK_F24).contains(&vk)
}

fn key_code(key: &str) -> Option<u16> {
    let mut chars = key.chars();
    if let (Some(c), None) = (chars.next(), chars.next())
        && c.is_ascii_alphanumeric()
    {
        return Some(c.to_ascii_uppercase() as u16);
    }
    if let Some(n) = key.strip_prefix('f')
        && let Ok(n) = n.parse::<u16>()
        && (1..=VK_F24 - VK_F1 + 1).contains(&n)
    {
        return Some(VK_F1 + n - 1);
    }
    NAMED_KEYS
        .iter()
        .find(|(name, _)| *name == key)
        .map(|(_, vk)| *vk)
}

fn key_name(vk: u16) -> String {
    if vk < 0x80 && matches!(vk as u8, b'0'..=b'9' | b'A'..=b'Z') {
        return (vk as u8 as char).to_string();
    }
    if is_function_key(vk) {
        return format!("F{}", vk - VK_F1 + 1);
    }
    NAMED_KEYS
        .iter()
        .find(|(_, code)| *code == vk)
        .map(|(name, _)| {
            let mut s = name.to_string();
            if let Some(first) = s.get_mut(0..1) {
                first.make_ascii_uppercase();
            }
            s
        })
        .unwrap_or_else(|| format!("VK_{vk:02X}"))
}

/// Canonical spelling, the way a menu shows an accelerator: `Ctrl+Alt+R`.
impl fmt::Display for Hotkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.ctrl {
            f.write_str("Ctrl+")?;
        }
        if self.alt {
            f.write_str("Alt+")?;
        }
        if self.shift {
            f.write_str("Shift+")?;
        }
        if self.win {
            f.write_str("Win+")?;
        }
        f.write_str(&key_name(self.vk))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chords_parse_case_and_space_insensitively() {
        let hk = Hotkey::parse("ctrl+alt+r").unwrap().unwrap();
        assert!(hk.ctrl && hk.alt && !hk.shift && !hk.win);
        assert_eq!(hk.vk, b'R' as u16);
        assert_eq!(hk.modifiers(), MOD_CONTROL | MOD_ALT);
        assert_eq!(hk.to_string(), "Ctrl+Alt+R");
        assert_eq!(Hotkey::parse(" Control + ALT + r ").unwrap(), Some(hk));
        let all = Hotkey::parse("ctrl+shift+alt+win+f9").unwrap().unwrap();
        assert_eq!(all.modifiers(), MOD_CONTROL | MOD_ALT | MOD_SHIFT | MOD_WIN);
        assert_eq!(all.vk, 0x78);
        assert_eq!(all.to_string(), "Ctrl+Alt+Shift+Win+F9");
        assert_eq!(Hotkey::parse("super+5").unwrap().unwrap().vk, b'5' as u16);
    }

    #[test]
    fn named_keys_and_function_keys() {
        assert_eq!(Hotkey::parse("ctrl+numpad0").unwrap().unwrap().vk, 0x60);
        assert_eq!(Hotkey::parse("shift+pageup").unwrap().unwrap().vk, 0x21);
        assert_eq!(Hotkey::parse("f24").unwrap().unwrap().vk, 0x87);
        assert_eq!(Hotkey::parse("F1").unwrap().unwrap().to_string(), "F1");
        assert_eq!(
            Hotkey::parse("alt+-").unwrap().unwrap().to_string(),
            "Alt+-"
        );
        assert_eq!(
            Hotkey::parse("ctrl+space").unwrap().unwrap().to_string(),
            "Ctrl+Space"
        );
        assert!(Hotkey::parse("f25").is_err());
        assert!(Hotkey::parse("f0").is_err());
    }

    #[test]
    fn standalone_keys_need_no_modifier_but_typing_keys_do() {
        assert!(Hotkey::parse("pause").unwrap().is_some());
        assert!(Hotkey::parse("scrolllock").unwrap().is_some());
        assert!(Hotkey::parse("f12").unwrap().is_some());
        let e = Hotkey::parse("r").unwrap_err();
        assert!(matches!(e, HotkeyError::PlainKey { .. }));
        assert!(e.to_string().contains("plain key"), "{e}");
        assert!(Hotkey::parse("5").is_err());
        assert!(Hotkey::parse("space").is_err());
    }

    #[test]
    fn empty_none_and_off_disable() {
        assert_eq!(Hotkey::parse("").unwrap(), None);
        assert_eq!(Hotkey::parse("  ").unwrap(), None);
        assert_eq!(Hotkey::parse("none").unwrap(), None);
        assert_eq!(Hotkey::parse("OFF").unwrap(), None);
    }

    #[test]
    fn errors_say_what_is_wrong() {
        let e = Hotkey::parse("ctrl+bogus").unwrap_err().to_string();
        assert!(
            e.starts_with("\"ctrl+bogus\": unknown key \"bogus\""),
            "{e}"
        );
        assert!(e.contains("numpad0") && e.contains("printscreen"), "{e}");
        let e = Hotkey::parse("r+ctrl").unwrap_err();
        assert_eq!(
            e,
            HotkeyError::NotAModifier {
                text: "r+ctrl".into(),
                part: "r".into()
            }
        );
        assert!(e.to_string().contains("not a modifier"), "{e}");
        let e = Hotkey::parse("ctrl++r").unwrap_err();
        assert!(e.to_string().contains("empty part"), "{e}");
        assert!(Hotkey::parse("ctrl+").is_err());
        assert!(
            Hotkey::parse("ctrl+alt").is_err(),
            "a modifier is not a key"
        );
    }
}
