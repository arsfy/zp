//! Keystrokes to the bytes a terminal program expects (xterm conventions).

use gpui::Keystroke;

/// xterm's modifier parameter: 1 + shift + 2·alt + 4·ctrl.
fn modifier_code(keystroke: &Keystroke) -> u8 {
    let m = &keystroke.modifiers;
    1 + u8::from(m.shift) + 2 * u8::from(m.alt) + 4 * u8::from(m.control)
}

/// The escape sequence for `keystroke`, or `None` when it is plain text (delivered by
/// the input handler, which also handles IME) or not for the terminal (e.g. ⌘ shortcuts).
/// `app_cursor`: the program asked for application cursor keys (DECCKM).
pub fn to_esc(keystroke: &Keystroke, app_cursor: bool) -> Option<String> {
    let m = &keystroke.modifiers;
    if m.platform {
        return None;
    }
    let code = modifier_code(keystroke);
    let modified = code > 1;
    // Cursor keys: `ESC [ 1 ; m X` with modifiers, `ESC O X` in application mode.
    let cursor = |letter: char| -> String {
        if modified {
            format!("\x1b[1;{code}{letter}")
        } else if app_cursor {
            format!("\x1bO{letter}")
        } else {
            format!("\x1b[{letter}")
        }
    };
    // F1–F4: `ESC O X`, or `ESC [ 1 ; m X` with modifiers.
    let function = |letter: char| -> String {
        if modified {
            format!("\x1b[1;{code}{letter}")
        } else {
            format!("\x1bO{letter}")
        }
    };
    // `ESC [ n ~` keys.
    let tilde = |n: u8| -> String {
        if modified {
            format!("\x1b[{n};{code}~")
        } else {
            format!("\x1b[{n}~")
        }
    };
    let alt_prefix = |text: &str| -> String {
        if m.alt {
            format!("\x1b{text}")
        } else {
            text.to_owned()
        }
    };
    let sequence = match keystroke.key.as_str() {
        "up" => cursor('A'),
        "down" => cursor('B'),
        "right" => cursor('C'),
        "left" => cursor('D'),
        "home" => cursor('H'),
        "end" => cursor('F'),
        "pageup" => tilde(5),
        "pagedown" => tilde(6),
        "insert" => tilde(2),
        "delete" => tilde(3),
        "f1" => function('P'),
        "f2" => function('Q'),
        "f3" => function('R'),
        "f4" => function('S'),
        "f5" => tilde(15),
        "f6" => tilde(17),
        "f7" => tilde(18),
        "f8" => tilde(19),
        "f9" => tilde(20),
        "f10" => tilde(21),
        "f11" => tilde(23),
        "f12" => tilde(24),
        "enter" => alt_prefix("\r"),
        "tab" if m.shift => "\x1b[Z".to_owned(),
        "tab" => alt_prefix("\t"),
        "escape" => "\x1b".to_owned(),
        "backspace" if m.control => alt_prefix("\x08"),
        "backspace" => alt_prefix("\x7f"),
        "space" if m.control => "\0".to_owned(),
        key if m.control => {
            let mut chars = key.chars();
            let (Some(c), None) = (chars.next(), chars.next()) else {
                return None;
            };
            let byte = match c.to_ascii_lowercase() {
                c @ 'a'..='z' => c as u8 - b'a' + 1,
                '@' | '2' => 0,
                '[' | '3' => 0x1b,
                '\\' | '4' => 0x1c,
                ']' | '5' => 0x1d,
                '^' | '6' => 0x1e,
                '_' | '-' | '7' => 0x1f,
                '/' => 0x1f,
                '8' | '?' => 0x7f,
                _ => return None,
            };
            alt_prefix(&char::from(byte).to_string())
        }
        // Option as Meta for letters and digits; other Option characters (e.g. `å`)
        // arrive as text.
        key if m.alt && key.len() == 1 && key.chars().all(|c| c.is_ascii_alphanumeric()) => {
            let text = if m.shift {
                key.to_ascii_uppercase()
            } else {
                key.to_owned()
            };
            format!("\x1b{text}")
        }
        _ => return None,
    };
    Some(sequence)
}

/// Text pasted into the terminal: bracketed when the program asked for it, otherwise
/// with line breaks as carriage returns.
pub fn paste(text: &str, bracketed: bool) -> String {
    if bracketed {
        // A pasted end marker would end the paste early.
        let text = text.replace("\x1b[201~", "");
        format!("\x1b[200~{text}\x1b[201~")
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn esc(keys: &str, app_cursor: bool) -> Option<String> {
        to_esc(&Keystroke::parse(keys).unwrap(), app_cursor)
    }

    #[test]
    fn special_keys() {
        assert_eq!(esc("enter", false).as_deref(), Some("\r"));
        assert_eq!(esc("backspace", false).as_deref(), Some("\x7f"));
        assert_eq!(esc("shift-tab", false).as_deref(), Some("\x1b[Z"));
        assert_eq!(esc("up", false).as_deref(), Some("\x1b[A"));
        assert_eq!(esc("up", true).as_deref(), Some("\x1bOA"));
        assert_eq!(esc("ctrl-left", false).as_deref(), Some("\x1b[1;5D"));
        assert_eq!(esc("alt-left", true).as_deref(), Some("\x1b[1;3D"));
        assert_eq!(esc("delete", false).as_deref(), Some("\x1b[3~"));
        assert_eq!(esc("shift-pageup", false).as_deref(), Some("\x1b[5;2~"));
        assert_eq!(esc("f1", false).as_deref(), Some("\x1bOP"));
        assert_eq!(esc("ctrl-f1", false).as_deref(), Some("\x1b[1;5P"));
        assert_eq!(esc("f5", false).as_deref(), Some("\x1b[15~"));
    }

    #[test]
    fn control_and_meta() {
        assert_eq!(esc("ctrl-c", false).as_deref(), Some("\x03"));
        assert_eq!(esc("ctrl-a", false).as_deref(), Some("\x01"));
        assert_eq!(esc("ctrl-[", false).as_deref(), Some("\x1b"));
        assert_eq!(esc("ctrl-space", false).as_deref(), Some("\0"));
        assert_eq!(esc("alt-b", false).as_deref(), Some("\x1bb"));
        assert_eq!(esc("alt-backspace", false).as_deref(), Some("\x1b\x7f"));
        // Plain text and ⌘ shortcuts are not escape sequences.
        assert_eq!(esc("a", false), None);
        assert_eq!(esc("cmd-c", false), None);
    }

    #[test]
    fn pasting() {
        assert_eq!(paste("a\nb\r\nc", false), "a\rb\rc");
        assert_eq!(paste("x", true), "\x1b[200~x\x1b[201~");
    }
}
