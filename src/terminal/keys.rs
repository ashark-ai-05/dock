use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KeyEncoding {
    /// DECCKM. Agent and editor TUIs switch this on, changing arrow keys from CSI to SS3.
    pub application_cursor: bool,
}

/// Translates a crossterm key into the bytes a PTY expects. Returns `None` for keys with
/// no terminal representation, which callers must drop rather than send as empty input.
pub fn encode_key(key: KeyEvent, encoding: KeyEncoding) -> Option<Vec<u8>> {
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    // Check if we have modifiers that trigger CSI modifier form (not just plain SHIFT).
    let has_modifier = key.modifiers != KeyModifiers::NONE && key.modifiers != KeyModifiers::SHIFT;

    let mut bytes = match key.code {
        KeyCode::Char(character) => {
            if control {
                vec![control_byte(character)?]
            } else {
                character.to_string().into_bytes()
            }
        }
        KeyCode::Enter => b"\r".to_vec(),
        KeyCode::Tab => b"\t".to_vec(),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Delete => {
            if has_modifier {
                let param = modifier_param(key.modifiers)?;
                format!("\x1b[3;{}~", param).into_bytes()
            } else {
                b"\x1b[3~".to_vec()
            }
        }
        KeyCode::Insert => {
            if has_modifier {
                let param = modifier_param(key.modifiers)?;
                format!("\x1b[2;{}~", param).into_bytes()
            } else {
                b"\x1b[2~".to_vec()
            }
        }
        KeyCode::PageUp => {
            if has_modifier {
                let param = modifier_param(key.modifiers)?;
                format!("\x1b[5;{}~", param).into_bytes()
            } else {
                b"\x1b[5~".to_vec()
            }
        }
        KeyCode::PageDown => {
            if has_modifier {
                let param = modifier_param(key.modifiers)?;
                format!("\x1b[6;{}~", param).into_bytes()
            } else {
                b"\x1b[6~".to_vec()
            }
        }
        KeyCode::Home => {
            if has_modifier {
                let param = modifier_param(key.modifiers)?;
                format!("\x1b[1;{}H", param).into_bytes()
            } else {
                cursor_key(b'H', encoding)
            }
        }
        KeyCode::End => {
            if has_modifier {
                let param = modifier_param(key.modifiers)?;
                format!("\x1b[1;{}F", param).into_bytes()
            } else {
                cursor_key(b'F', encoding)
            }
        }
        KeyCode::Up => {
            if has_modifier {
                let param = modifier_param(key.modifiers)?;
                format!("\x1b[1;{}A", param).into_bytes()
            } else {
                cursor_key(b'A', encoding)
            }
        }
        KeyCode::Down => {
            if has_modifier {
                let param = modifier_param(key.modifiers)?;
                format!("\x1b[1;{}B", param).into_bytes()
            } else {
                cursor_key(b'B', encoding)
            }
        }
        KeyCode::Right => {
            if has_modifier {
                let param = modifier_param(key.modifiers)?;
                format!("\x1b[1;{}C", param).into_bytes()
            } else {
                cursor_key(b'C', encoding)
            }
        }
        KeyCode::Left => {
            if has_modifier {
                let param = modifier_param(key.modifiers)?;
                format!("\x1b[1;{}D", param).into_bytes()
            } else {
                cursor_key(b'D', encoding)
            }
        }
        KeyCode::F(number) => {
            if has_modifier {
                let param = modifier_param(key.modifiers)?;
                function_key_with_modifier(number, param)?
            } else {
                function_key(number)?
            }
        }
        _ => return None,
    };

    // For plain characters with ALT, prepend ESC (unless already a control char).
    if alt && matches!(key.code, KeyCode::Char(_)) && !control {
        bytes.insert(0, 0x1b);
    }

    Some(bytes)
}

/// Wraps pasted text so the receiving application can tell it apart from typing.
pub fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    if !bracketed {
        return text.as_bytes().to_vec();
    }
    let mut bytes = b"\x1b[200~".to_vec();
    // Filter out the terminator sequence to prevent paste injection attacks.
    let text_bytes = text.as_bytes();
    let terminator = b"\x1b[201~";
    let mut i = 0;
    while i < text_bytes.len() {
        if text_bytes[i..].starts_with(terminator) {
            i += terminator.len();
        } else {
            bytes.push(text_bytes[i]);
            i += 1;
        }
    }
    bytes.extend_from_slice(b"\x1b[201~");
    bytes
}

fn modifier_param(modifiers: KeyModifiers) -> Option<u8> {
    // Modifier parameter: 1 + shift(1) + alt(2) + ctrl(4).
    if modifiers == KeyModifiers::NONE || modifiers == KeyModifiers::SHIFT {
        return None;
    }
    let mut param = 1u8;
    if modifiers.contains(KeyModifiers::SHIFT) {
        param += 1;
    }
    if modifiers.contains(KeyModifiers::ALT) {
        param += 2;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        param += 4;
    }
    Some(param)
}

fn control_byte(character: char) -> Option<u8> {
    match character {
        ' ' => Some(0),
        'a'..='z' => Some(character as u8 - b'a' + 1),
        'A'..='Z' => Some(character as u8 - b'A' + 1),
        // crossterm sends bytes 0x1C-0x1F as Char('4')-Char('7') with CONTROL.
        '4'..='7' => Some(character as u8 - b'4' + 0x1C),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' => Some(0x1f),
        '?' => Some(0x7f),
        _ => None,
    }
}

fn cursor_key(final_byte: u8, encoding: KeyEncoding) -> Vec<u8> {
    let introducer: &[u8] = if encoding.application_cursor {
        b"\x1bO"
    } else {
        b"\x1b["
    };
    let mut bytes = introducer.to_vec();
    bytes.push(final_byte);
    bytes
}

fn function_key(number: u8) -> Option<Vec<u8>> {
    let sequence: &[u8] = match number {
        1 => b"\x1bOP",
        2 => b"\x1bOQ",
        3 => b"\x1bOR",
        4 => b"\x1bOS",
        5 => b"\x1b[15~",
        6 => b"\x1b[17~",
        7 => b"\x1b[18~",
        8 => b"\x1b[19~",
        9 => b"\x1b[20~",
        10 => b"\x1b[21~",
        11 => b"\x1b[23~",
        12 => b"\x1b[24~",
        _ => return None,
    };
    Some(sequence.to_vec())
}

fn function_key_with_modifier(number: u8, param: u8) -> Option<Vec<u8>> {
    // F1-F4 become CSI form when modified; F5+ already use tilde form.
    let sequence = match number {
        1 => format!("\x1b[1;{}P", param),
        2 => format!("\x1b[1;{}Q", param),
        3 => format!("\x1b[1;{}R", param),
        4 => format!("\x1b[1;{}S", param),
        5 => format!("\x1b[15;{}~", param),
        6 => format!("\x1b[17;{}~", param),
        7 => format!("\x1b[18;{}~", param),
        8 => format!("\x1b[19;{}~", param),
        9 => format!("\x1b[20;{}~", param),
        10 => format!("\x1b[21;{}~", param),
        11 => format!("\x1b[23;{}~", param),
        12 => format!("\x1b[24;{}~", param),
        _ => return None,
    };
    Some(sequence.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn normal() -> KeyEncoding {
        KeyEncoding {
            application_cursor: false,
        }
    }

    fn encode(code: KeyCode, modifiers: KeyModifiers) -> Vec<u8> {
        encode_key(KeyEvent::new(code, modifiers), normal()).expect("encodable key")
    }

    #[test]
    fn encodes_plain_characters_and_control_combinations() {
        assert_eq!(encode(KeyCode::Char('a'), KeyModifiers::NONE), b"a");
        assert_eq!(
            encode(KeyCode::Char('c'), KeyModifiers::CONTROL),
            vec![0x03]
        );
        assert_eq!(
            encode(KeyCode::Char('b'), KeyModifiers::CONTROL),
            vec![0x02]
        );
        assert_eq!(
            encode(KeyCode::Char('a'), KeyModifiers::ALT),
            vec![0x1b, b'a']
        );
    }

    #[test]
    fn encodes_named_keys_that_agent_tuis_depend_on() {
        assert_eq!(encode(KeyCode::Esc, KeyModifiers::NONE), vec![0x1b]);
        assert_eq!(encode(KeyCode::Enter, KeyModifiers::NONE), b"\r");
        assert_eq!(encode(KeyCode::Tab, KeyModifiers::NONE), b"\t");
        assert_eq!(encode(KeyCode::BackTab, KeyModifiers::NONE), b"\x1b[Z");
        assert_eq!(encode(KeyCode::Backspace, KeyModifiers::NONE), vec![0x7f]);
        assert_eq!(encode(KeyCode::Delete, KeyModifiers::NONE), b"\x1b[3~");
        assert_eq!(encode(KeyCode::Home, KeyModifiers::NONE), b"\x1b[H");
        assert_eq!(encode(KeyCode::End, KeyModifiers::NONE), b"\x1b[F");
        assert_eq!(encode(KeyCode::PageUp, KeyModifiers::NONE), b"\x1b[5~");
        assert_eq!(encode(KeyCode::F(1), KeyModifiers::NONE), b"\x1bOP");
        assert_eq!(encode(KeyCode::F(5), KeyModifiers::NONE), b"\x1b[15~");
    }

    #[test]
    fn arrow_keys_respect_application_cursor_mode() {
        assert_eq!(encode(KeyCode::Up, KeyModifiers::NONE), b"\x1b[A");
        let app = KeyEncoding {
            application_cursor: true,
        };
        let up = encode_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), app).unwrap();
        assert_eq!(up, b"\x1bOA");
    }

    #[test]
    fn bracketed_paste_wraps_text_only_when_enabled() {
        assert_eq!(encode_paste("hi", true), b"\x1b[200~hi\x1b[201~".to_vec());
        assert_eq!(encode_paste("hi", false), b"hi".to_vec());
    }

    #[test]
    fn bracketed_paste_filters_terminator() {
        // Paste content with embedded terminator sequence must not allow injection.
        let result = encode_paste("safe\x1b[201~rm -rf /", true);
        let terminator = b"\x1b[201~";
        let occurrences = result
            .windows(terminator.len())
            .filter(|w| *w == terminator)
            .count();
        assert_eq!(occurrences, 1, "Should have exactly one terminator");
        assert_eq!(
            &result[result.len() - terminator.len()..],
            terminator,
            "Terminator should be the final bytes"
        );
    }

    #[test]
    fn encodes_control_digits() {
        // crossterm sends bytes 0x1C-0x1F as Char('4')-Char('7') with CONTROL.
        assert_eq!(
            encode(KeyCode::Char('4'), KeyModifiers::CONTROL),
            vec![0x1C],
            "Ctrl+4 should be 0x1C"
        );
        assert_eq!(
            encode(KeyCode::Char('5'), KeyModifiers::CONTROL),
            vec![0x1D],
            "Ctrl+5 should be 0x1D"
        );
        assert_eq!(
            encode(KeyCode::Char('6'), KeyModifiers::CONTROL),
            vec![0x1E],
            "Ctrl+6 should be 0x1E"
        );
        assert_eq!(
            encode(KeyCode::Char('7'), KeyModifiers::CONTROL),
            vec![0x1F],
            "Ctrl+7 should be 0x1F"
        );
    }

    #[test]
    fn modified_keys_use_csi_form() {
        let normal = KeyEncoding {
            application_cursor: false,
        };

        // Alt+Left should emit CSI modifier form, not double-ESC.
        let alt_left = encode_key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT), normal).unwrap();
        assert_eq!(alt_left, b"\x1b[1;3D");

        let alt_up = encode_key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT), normal).unwrap();
        assert_eq!(alt_up, b"\x1b[1;3A");

        let ctrl_right =
            encode_key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL), normal).unwrap();
        assert_eq!(ctrl_right, b"\x1b[1;5C");

        let alt_delete =
            encode_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::ALT), normal).unwrap();
        assert_eq!(alt_delete, b"\x1b[3;3~");

        let alt_f1 = encode_key(KeyEvent::new(KeyCode::F(1), KeyModifiers::ALT), normal).unwrap();
        assert_eq!(alt_f1, b"\x1b[1;3P");

        // Plain Alt+a should still prepend ESC (standard for characters).
        let alt_a =
            encode_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT), normal).unwrap();
        assert_eq!(alt_a, b"\x1ba");

        // Unmodified Up should still respect DECCKM (normal mode).
        let up = encode_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), normal).unwrap();
        assert_eq!(up, b"\x1b[A");

        let app_mode = KeyEncoding {
            application_cursor: true,
        };
        let up_app = encode_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), app_mode).unwrap();
        assert_eq!(up_app, b"\x1bOA");
    }
}
