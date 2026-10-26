//! Parsing shortcut strings like "win+shift+d" into RegisterHotKey modifiers and key codes.

pub const MOD_ALT: u32 = 0x1;
pub const MOD_CONTROL: u32 = 0x2;
pub const MOD_SHIFT: u32 = 0x4;
pub const MOD_WIN: u32 = 0x8;

fn modifier(name: &str) -> Option<u32> {
    match name {
        "ctrl" | "control" => Some(MOD_CONTROL),
        "alt" => Some(MOD_ALT),
        "shift" => Some(MOD_SHIFT),
        "win" | "windows" | "super" | "meta" => Some(MOD_WIN),
        _ => None,
    }
}

/// Virtual-key code for a key name.
fn key(name: &str) -> Option<u32> {
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.next())
        && c.is_ascii_alphanumeric()
    {
        return Some(c.to_ascii_uppercase() as u32);
    }
    if let Some(n) = name.strip_prefix('f').and_then(|n| n.parse::<u32>().ok())
        && (1..=24).contains(&n)
    {
        return Some(0x70 + n - 1);
    }
    Some(match name {
        "space" => 0x20,
        "tab" => 0x09,
        "enter" | "return" => 0x0D,
        "esc" | "escape" => 0x1B,
        "backspace" => 0x08,
        "insert" => 0x2D,
        "delete" | "del" => 0x2E,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" => 0x21,
        "pagedown" => 0x22,
        "left" => 0x25,
        "up" => 0x26,
        "right" => 0x27,
        "down" => 0x28,
        "`" | "backtick" => 0xC0,
        "-" | "minus" => 0xBD,
        "=" | "equals" => 0xBB,
        "," | "comma" => 0xBC,
        "." | "period" => 0xBE,
        "/" | "slash" => 0xBF,
        ";" | "semicolon" => 0xBA,
        "'" | "quote" => 0xDE,
        "[" => 0xDB,
        "]" => 0xDD,
        "\\" | "backslash" => 0xDC,
        _ => return None,
    })
}

fn parts(s: &str) -> Vec<String> {
    s.split('+').map(|p| p.trim().to_lowercase()).filter(|p| !p.is_empty()).collect()
}

/// Modifiers only, e.g. "ctrl+alt" (the digit is added per app). `Ok(None)` when empty.
pub fn parse_modifiers(s: &str) -> Result<Option<u32>, String> {
    let parts = parts(s);
    if parts.is_empty() {
        return Ok(None);
    }
    let mut mods = 0;
    for p in &parts {
        mods |= modifier(p).ok_or_else(|| format!("\"{p}\" isn't a modifier (use ctrl, alt, shift, win)"))?;
    }
    Ok(Some(mods))
}

/// Modifiers plus one key, e.g. "win+shift+d". `Ok(None)` when empty.
pub fn parse_shortcut(s: &str) -> Result<Option<(u32, u32)>, String> {
    let parts = parts(s);
    if parts.is_empty() {
        return Ok(None);
    }
    let (mut mods, mut vk) = (0, None);
    for p in &parts {
        match (modifier(p), key(p)) {
            (Some(m), _) => mods |= m,
            (None, Some(k)) if vk.is_none() => vk = Some(k),
            (None, Some(_)) => return Err("a shortcut can only have one key besides the modifiers".into()),
            (None, None) => return Err(format!("unknown key \"{p}\"")),
        }
    }
    let vk = vk.ok_or("the shortcut needs a key, e.g. \"win+shift+d\"")?;
    if mods == 0 {
        return Err("the shortcut needs at least one of ctrl, alt, shift or win".into());
    }
    Ok(Some((mods, vk)))
}

/// Human-readable form, e.g. "Ctrl+Alt+1".
pub fn describe(mods: u32, vk: u32) -> String {
    let mut s = String::new();
    for (bit, name) in [(MOD_WIN, "Win"), (MOD_CONTROL, "Ctrl"), (MOD_ALT, "Alt"), (MOD_SHIFT, "Shift")] {
        if mods & bit != 0 {
            s.push_str(name);
            s.push('+');
        }
    }
    match vk {
        0x30..=0x39 | 0x41..=0x5A => s.push(vk as u8 as char),
        0x70..=0x87 => s.push_str(&format!("F{}", vk - 0x70 + 1)),
        0x20 => s.push_str("Space"),
        _ => s.push_str(&format!("key {vk:#x}")),
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_shortcuts() {
        assert_eq!(parse_shortcut("win+shift+d"), Ok(Some((MOD_WIN | MOD_SHIFT, 'D' as u32))));
        assert_eq!(parse_shortcut(" Ctrl + Alt + F5 "), Ok(Some((MOD_CONTROL | MOD_ALT, 0x74))));
        assert_eq!(parse_shortcut(""), Ok(None));
        assert!(parse_shortcut("d").is_err(), "needs a modifier");
        assert!(parse_shortcut("ctrl+alt").is_err(), "needs a key");
        assert!(parse_shortcut("ctrl+a+b").is_err(), "one key only");
        assert!(parse_shortcut("ctrl+banana").is_err());
    }

    #[test]
    fn parses_modifiers() {
        assert_eq!(parse_modifiers("ctrl+alt"), Ok(Some(MOD_CONTROL | MOD_ALT)));
        assert_eq!(parse_modifiers("  "), Ok(None));
        assert!(parse_modifiers("ctrl+1").is_err());
    }

    #[test]
    fn describes_shortcuts() {
        assert_eq!(describe(MOD_CONTROL | MOD_ALT, '1' as u32), "Ctrl+Alt+1");
        assert_eq!(describe(MOD_WIN | MOD_SHIFT, 'D' as u32), "Win+Shift+D");
    }
}
