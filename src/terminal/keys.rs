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
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Home => cursor_key(b'H', encoding),
        KeyCode::End => cursor_key(b'F', encoding),
        KeyCode::Up => cursor_key(b'A', encoding),
        KeyCode::Down => cursor_key(b'B', encoding),
        KeyCode::Right => cursor_key(b'C', encoding),
        KeyCode::Left => cursor_key(b'D', encoding),
        KeyCode::F(number) => function_key(number)?,
        _ => return None,
    };
    if alt {
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
    bytes.extend_from_slice(text.as_bytes());
    bytes.extend_from_slice(b"\x1b[201~");
    bytes
}

fn control_byte(character: char) -> Option<u8> {
    match character {
        ' ' => Some(0),
        'a'..='z' => Some(character as u8 - b'a' + 1),
        'A'..='Z' => Some(character as u8 - b'A' + 1),
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
}
