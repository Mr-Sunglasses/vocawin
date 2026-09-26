//! Hotkey presets and parsing for VocaWin.
//!
//! Lone modifiers (Right Ctrl / Right Alt / Right Shift) cannot be bound with
//! RegisterHotKey. They are first-class presets here and are watched by the
//! WH_KEYBOARD_LL hook in `hook`.

/// Built-in presets shown in Settings. Values are stored in settings.json.
/// Right Alt is first: it matches VocaLinux hold-default (PTT). The hook leaves
/// AltGr (Ctrl+Right Alt) alone so layout characters still type.
pub const PRESETS: &[(&str, &str)] = &[
    ("AltRight", "Right Alt (Option)"),
    ("ControlRight", "Right Ctrl"),
    ("ShiftRight", "Right Shift"),
    ("F8", "F8"),
    ("F9", "F9"),
    ("F10", "F10"),
    ("Ctrl+Alt+Space", "Ctrl+Alt+Space"),
    ("Ctrl+Shift+Space", "Ctrl+Shift+Space"),
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HotkeySpec {
    /// A single key, including a side-specific modifier (VK_RCONTROL, …).
    Lone { vk: u32 },
    /// Modifier chord + key. Win/Super is rejected at parse time.
    Combo {
        ctrl: bool,
        alt: bool,
        shift: bool,
        vk: u32,
    },
}

// Virtual-key codes (winuser.h). Kept as u32 so non-Windows CI can parse too.
pub const VK_SPACE: u32 = 0x20;
pub const VK_LSHIFT: u32 = 0xA0;
pub const VK_RSHIFT: u32 = 0xA1;
pub const VK_LCONTROL: u32 = 0xA2;
pub const VK_RCONTROL: u32 = 0xA3;
pub const VK_LMENU: u32 = 0xA4;
pub const VK_RMENU: u32 = 0xA5;
pub const VK_F7: u32 = 0x76;
pub const VK_F8: u32 = 0x77;
pub const VK_F9: u32 = 0x78;
pub const VK_F10: u32 = 0x79;

/// Side-specific or generic Ctrl, Alt, or Shift.
pub fn is_modifier_vk(vk: u32) -> bool {
    matches!(
        vk,
        0x10 | 0x11 | 0x12 | VK_LSHIFT | VK_RSHIFT | VK_LCONTROL | VK_RCONTROL | VK_LMENU | VK_RMENU
    )
}

pub fn parse_hotkey(spec: &str) -> Result<HotkeySpec, String> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return Err("Hotkey is empty".into());
    }
    match trimmed {
        "ControlRight" | "Right Ctrl" | "RControl" | "RCtrl" => {
            Ok(HotkeySpec::Lone { vk: VK_RCONTROL })
        }
        "ControlLeft" | "Left Ctrl" | "LControl" | "LCtrl" => {
            Ok(HotkeySpec::Lone { vk: VK_LCONTROL })
        }
        "AltRight" | "Right Alt" | "Right Alt (Option)" | "RAlt" | "ROption" | "Option" => {
            Ok(HotkeySpec::Lone { vk: VK_RMENU })
        }
        "AltLeft" | "Left Alt" | "LAlt" => Ok(HotkeySpec::Lone { vk: VK_LMENU }),
        "ShiftRight" | "Right Shift" | "RShift" => Ok(HotkeySpec::Lone { vk: VK_RSHIFT }),
        "ShiftLeft" | "Left Shift" | "LShift" => Ok(HotkeySpec::Lone { vk: VK_LSHIFT }),
        "F7" => Ok(HotkeySpec::Lone { vk: VK_F7 }),
        "F8" => Ok(HotkeySpec::Lone { vk: VK_F8 }),
        "F9" => Ok(HotkeySpec::Lone { vk: VK_F9 }),
        "F10" => Ok(HotkeySpec::Lone { vk: VK_F10 }),
        "Space" => Ok(HotkeySpec::Lone { vk: VK_SPACE }),
        other => parse_combo(other),
    }
}

fn parse_combo(spec: &str) -> Result<HotkeySpec, String> {
    let mut ctrl = false;
    let mut alt = false;
    let mut shift = false;
    let mut key: Option<u32> = None;
    for raw in spec.split('+') {
        let part = raw.trim();
        if part.is_empty() {
            continue;
        }
        let lower = part.to_ascii_lowercase();
        match lower.as_str() {
            "ctrl" | "control" | "controlleft" | "controlright" | "lctrl" | "rctrl" => {
                ctrl = true;
            }
            "alt" | "option" | "altleft" | "altright" | "lalt" | "ralt" => alt = true,
            "shift" | "shiftleft" | "shiftright" | "lshift" | "rshift" => shift = true,
            "meta" | "win" | "super" | "cmd" | "command" | "windows" => {
                return Err(
                    "Win/Super shortcuts are reserved on Windows. Pick Right Alt, Right Ctrl, a function key, or another combo."
                        .into(),
                );
            }
            other => match key_vk(other) {
                Some(vk) => key = Some(vk),
                None if other.chars().count() == 1 => {
                    return Err(format!("Unsupported hotkey key '{part}'"))
                }
                None => return Err(format!("Unsupported hotkey part '{part}'")),
            },
        }
    }
    let vk = key.ok_or_else(|| format!("Hotkey '{spec}' is missing a key"))?;
    if !ctrl && !alt && !shift {
        return Ok(HotkeySpec::Lone { vk });
    }
    Ok(HotkeySpec::Combo {
        ctrl,
        alt,
        shift,
        vk,
    })
}

#[allow(dead_code)]
pub fn display_name(spec: &str) -> String {
    for (id, label) in PRESETS {
        if parse_hotkey(id).ok() == parse_hotkey(spec).ok() {
            return (*label).to_string();
        }
        if spec.eq_ignore_ascii_case(id) || spec.eq_ignore_ascii_case(label) {
            return (*label).to_string();
        }
    }
    format!("Custom: {spec}")
}

/// Normalize a recorded/frontend combo into the canonical settings string.
pub fn canonicalize(spec: &str) -> Result<String, String> {
    let parsed = parse_hotkey(spec)?;
    for (id, _) in PRESETS {
        if parse_hotkey(id).ok() == Some(parsed.clone()) {
            return Ok((*id).to_string());
        }
    }
    Ok(match parsed {
        HotkeySpec::Lone { vk } => lone_id(vk).unwrap_or_else(|| spec.to_string()),
        HotkeySpec::Combo {
            ctrl,
            alt,
            shift,
            vk,
        } => {
            let mut parts = Vec::new();
            if ctrl {
                parts.push("Ctrl".to_string());
            }
            if alt {
                parts.push("Alt".to_string());
            }
            if shift {
                parts.push("Shift".to_string());
            }
            parts.push(key_token(vk));
            parts.join("+")
        }
    })
}

fn lone_id(vk: u32) -> Option<String> {
    Some(
        match vk {
            VK_RCONTROL => "ControlRight",
            VK_LCONTROL => "ControlLeft",
            VK_RMENU => "AltRight",
            VK_LMENU => "AltLeft",
            VK_RSHIFT => "ShiftRight",
            VK_LSHIFT => "ShiftLeft",
            VK_F7 => "F7",
            VK_F8 => "F8",
            VK_F9 => "F9",
            VK_F10 => "F10",
            VK_SPACE => "Space",
            _ => return None,
        }
        .to_string(),
    )
}

fn key_token(vk: u32) -> String {
    key_name(vk).unwrap_or_else(|| format!("Vk{vk}"))
}

/// Named keys besides letters, digits and F1-F24, with their virtual-key
/// codes. Punctuation uses the US-layout character on the key.
const NAMED_KEYS: &[(&str, u32)] = &[
    ("Space", VK_SPACE),
    ("PageUp", 0x21),
    ("PageDown", 0x22),
    ("End", 0x23),
    ("Home", 0x24),
    ("Left", 0x25),
    ("Up", 0x26),
    ("Right", 0x27),
    ("Down", 0x28),
    ("Insert", 0x2D),
    ("Delete", 0x2E),
    ("Pause", 0x13),
    ("ScrollLock", 0x91),
    (";", 0xBA),
    ("=", 0xBB),
    (",", 0xBC),
    ("-", 0xBD),
    (".", 0xBE),
    ("/", 0xBF),
    ("`", 0xC0),
    ("[", 0xDB),
    ("\\", 0xDC),
    ("]", 0xDD),
    ("'", 0xDE),
];

/// The settings name of a key a shortcut can use, if it has one.
fn key_name(vk: u32) -> Option<String> {
    match vk {
        0x30..=0x39 | 0x41..=0x5A => char::from_u32(vk).map(|ch| ch.to_string()),
        0x70..=0x87 => Some(format!("F{}", vk - 0x6F)),
        _ => NAMED_KEYS
            .iter()
            .find(|(_, code)| *code == vk)
            .map(|(name, _)| (*name).to_string()),
    }
}

/// The virtual-key code for a key name (any case).
fn key_vk(name: &str) -> Option<u32> {
    let upper = name.to_ascii_uppercase();
    let mut chars = upper.chars();
    if let (Some(ch), None) = (chars.next(), chars.next()) {
        if ch.is_ascii_alphanumeric() {
            return Some(ch as u32);
        }
    }
    if let Some(number) = upper.strip_prefix('F').and_then(|rest| rest.parse::<u32>().ok()) {
        if (1..=24).contains(&number) {
            return Some(0x6F + number);
        }
    }
    NAMED_KEYS
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, vk)| *vk)
}

/// Keys that are fine to hold on their own: they type nothing and move
/// nothing, so eating them while dictating costs the user no input.
fn lone_key_is_safe(vk: u32) -> bool {
    matches!(vk, 0x70..=0x87 | 0x2D | 0x13 | 0x91)
}

/// The shortcut for keys held together while recording one in Settings:
/// a lone Right/Left Ctrl, Alt or Shift, or modifiers plus one key. A key
/// that types or moves the caret needs Ctrl or Alt, since dictation eats it
/// while held.
pub fn from_keys(ctrl: bool, alt: bool, shift: bool, vk: u32) -> Result<String, String> {
    if is_modifier_vk(vk) {
        return lone_id(vk).ok_or_else(|| "That modifier cannot be a shortcut.".to_string());
    }
    let Some(name) = key_name(vk) else {
        return Err("That key cannot be part of a shortcut. Try a letter, number, F-key or Space with Ctrl or Alt.".into());
    };
    if !ctrl && !alt && !lone_key_is_safe(vk) {
        return Err(format!(
            "{name} types or moves the caret. Hold Ctrl or Alt with it, for example Ctrl+{name}."
        ));
    }
    let mut parts = Vec::new();
    if ctrl {
        parts.push("Ctrl".to_string());
    }
    if alt {
        parts.push("Alt".to_string());
    }
    if shift {
        parts.push("Shift".to_string());
    }
    parts.push(name);
    canonicalize(&parts.join("+"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_all_parse() {
        for (id, _) in PRESETS {
            parse_hotkey(id).unwrap_or_else(|error| panic!("{id}: {error}"));
        }
    }

    #[test]
    fn right_alt_is_lone_rmenu() {
        assert_eq!(
            parse_hotkey("AltRight").unwrap(),
            HotkeySpec::Lone { vk: VK_RMENU }
        );
        assert_eq!(canonicalize("Right Alt").unwrap(), "AltRight");
        assert_eq!(canonicalize("Option").unwrap(), "AltRight");
    }

    #[test]
    fn right_ctrl_is_lone_rcontrol() {
        assert_eq!(
            parse_hotkey("ControlRight").unwrap(),
            HotkeySpec::Lone { vk: VK_RCONTROL }
        );
        assert_eq!(canonicalize("Right Ctrl").unwrap(), "ControlRight");
    }

    #[test]
    fn right_shift_and_f_keys_parse() {
        assert_eq!(
            parse_hotkey("ShiftRight").unwrap(),
            HotkeySpec::Lone { vk: VK_RSHIFT }
        );
        assert_eq!(parse_hotkey("F9").unwrap(), HotkeySpec::Lone { vk: VK_F9 });
    }

    #[test]
    fn default_combo_still_parses() {
        assert_eq!(
            parse_hotkey("Ctrl+Alt+Space").unwrap(),
            HotkeySpec::Combo {
                ctrl: true,
                alt: true,
                shift: false,
                vk: VK_SPACE
            }
        );
    }

    #[test]
    fn recorded_keys_become_shortcuts() {
        assert_eq!(from_keys(true, false, false, VK_SPACE).unwrap(), "Ctrl+Space");
        assert_eq!(from_keys(true, true, false, VK_SPACE).unwrap(), "Ctrl+Alt+Space");
        assert_eq!(from_keys(false, true, true, 0x44).unwrap(), "Alt+Shift+D");
        assert_eq!(from_keys(false, false, false, VK_F9).unwrap(), "F9");
        assert_eq!(from_keys(false, false, false, 0x7B).unwrap(), "F12");
        assert_eq!(from_keys(true, false, false, 0xC0).unwrap(), "Ctrl+`");
        assert_eq!(from_keys(false, false, false, VK_RCONTROL).unwrap(), "ControlRight");
        assert_eq!(from_keys(false, false, false, VK_LMENU).unwrap(), "AltLeft");
    }

    #[test]
    fn keys_that_type_need_ctrl_or_alt() {
        for vk in [VK_SPACE, 0x41, 0x35, 0x25, 0xBC] {
            assert!(from_keys(false, false, false, vk).is_err(), "{vk:#x}");
            assert!(from_keys(false, false, true, vk).is_err(), "{vk:#x}");
        }
        assert!(from_keys(true, false, false, 0x0D).is_err());
    }

    #[test]
    fn new_key_names_round_trip() {
        for spec in ["Ctrl+F12", "Alt+PageDown", "Ctrl+Shift+/", "F24", "Ctrl+\\", "Ctrl+Alt+Left"] {
            assert_eq!(canonicalize(spec).unwrap(), spec, "{spec}");
        }
        assert_eq!(canonicalize("ctrl+space").unwrap(), "Ctrl+Space");
        assert_eq!(canonicalize("F8").unwrap(), "F8");
    }

    #[test]
    fn win_super_is_rejected() {
        assert!(parse_hotkey("Super+Space").unwrap_err().contains("reserved"));
        assert!(parse_hotkey("Win+A").unwrap_err().contains("reserved"));
    }
}
