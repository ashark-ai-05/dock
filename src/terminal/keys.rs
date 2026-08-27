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
    // Filter terminator sequences from the payload. A single forward pass can miss
    // terminators formed by fragments recombining after removal, so re-check after
    // each byte. This fixed-point approach guarantees no terminator can survive.
    const PASTE_END: &[u8] = b"\x1b[201~";
    let mut cleaned = Vec::with_capacity(text.len());
    for &byte in text.as_bytes() {
        cleaned.push(byte);
        // Re-check after every byte: removing one terminator can leave fragments that
        // concatenate into another, so a single forward pass is bypassable.
        if cleaned.ends_with(PASTE_END) {
            cleaned.truncate(cleaned.len() - PASTE_END.len());
        }
    }
    bytes.extend_from_slice(&cleaned);
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

    // Helper to verify bracketed paste contains exactly one terminator at the end.
    fn assert_paste_safe(result: &[u8]) {
        let terminator = b"\x1b[201~";
        let occurrences = result
            .windows(terminator.len())
            .filter(|w| *w == terminator)
            .count();
        assert_eq!(occurrences, 1, "Should have exactly one terminator");
        assert!(
            result.ends_with(terminator),
            "Terminator should be the final bytes"
        );
    }

    #[test]
    fn bracketed_paste_filters_single_terminator() {
        // Single terminator embedded in payload.
        let result = encode_paste("safe\x1b[201~rm -rf /", true);
        assert_paste_safe(&result);
    }

    #[test]
    fn bracketed_paste_filters_fragment_reconstruction_attack() {
        // Split terminator: removing the interior match leaves fragments that
        // recombine into a fresh terminator. The fixed-point algorithm catches this.
        let result = encode_paste("\x1b[20\x1b[201~1~", true);
        assert_paste_safe(&result);
    }

    #[test]
    fn bracketed_paste_filters_fragment_reconstruction_mid_payload() {
        // Same fragment reconstruction attack but embedded mid-payload.
        let result = encode_paste("AAA\x1b[20\x1b[201~1~BBB", true);
        assert_paste_safe(&result);
    }

    #[test]
    fn bracketed_paste_filters_multiple_adjacent_terminators() {
        // Several adjacent terminator sequences.
        let result = encode_paste("\x1b[201~\x1b[201~\x1b[201~", true);
        assert_paste_safe(&result);
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

/// What a pane's program has asked to be told about the mouse.
///
/// Read off the pane's own replica rather than assumed, exactly as `application_cursor` is. An
/// alt-screen TUI — Amp, Copilot, an editor — turns mouse reporting on and scrolls *itself*;
/// Dock was keeping every wheel notch for its own scrollback, which that pane does not have
/// (`vt100` gives the alternate grid no scrollback at all), so the wheel did nothing at all in
/// exactly the panes a person most wants to scroll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseEncoding {
    pub mode: vt100::MouseProtocolMode,
    pub encoding: vt100::MouseProtocolEncoding,
}

impl MouseEncoding {
    /// Whether the program wants to hear about the mouse at all.
    pub fn wanted(self) -> bool {
        self.mode != vt100::MouseProtocolMode::None
    }

    /// Whether it wants motion reported for this event, which most do not.
    fn wants_motion(self, dragging: bool) -> bool {
        match self.mode {
            vt100::MouseProtocolMode::None | vt100::MouseProtocolMode::Press => false,
            vt100::MouseProtocolMode::PressRelease => false,
            vt100::MouseProtocolMode::ButtonMotion => dragging,
            vt100::MouseProtocolMode::AnyMotion => true,
        }
    }

    /// Whether it wants button releases. X10 mode reports presses only.
    fn wants_release(self) -> bool {
        !matches!(
            self.mode,
            vt100::MouseProtocolMode::None | vt100::MouseProtocolMode::Press
        )
    }
}

/// Encodes a mouse event for a pane's program, in the protocol that program asked for.
///
/// `column` and `row` are zero-based cells *within the pane*, which the caller resolves; the
/// wire format is one-based, so both are incremented here — one place, rather than at each of
/// the two encodings.
///
/// Returns `None` when the program does not want this event, which the caller must treat as
/// "keep it for Dock" rather than as "send nothing": a wheel notch a program has not asked for
/// is a notch that should scroll Dock's own scrollback.
pub fn encode_mouse(
    kind: crossterm::event::MouseEventKind,
    modifiers: KeyModifiers,
    column: u16,
    row: u16,
    encoding: MouseEncoding,
) -> Option<Vec<u8>> {
    use crossterm::event::MouseEventKind;

    if !encoding.wanted() {
        return None;
    }
    // Button numbers are xterm's: 0/1/2 for left/middle/right, +32 for motion, 64/65 for the
    // wheel. Modifiers are a bitfield above that — shift 4, alt 8, control 16.
    let (mut button, release) = match kind {
        MouseEventKind::Down(button) => (mouse_button(button), false),
        MouseEventKind::Up(button) => {
            if !encoding.wants_release() {
                return None;
            }
            (mouse_button(button), true)
        }
        MouseEventKind::Drag(button) => {
            if !encoding.wants_motion(true) {
                return None;
            }
            (mouse_button(button) + 32, false)
        }
        MouseEventKind::Moved => {
            if !encoding.wants_motion(false) {
                return None;
            }
            (35, false)
        }
        MouseEventKind::ScrollUp => (64, false),
        MouseEventKind::ScrollDown => (65, false),
        MouseEventKind::ScrollLeft => (66, false),
        MouseEventKind::ScrollRight => (67, false),
    };
    if modifiers.contains(KeyModifiers::SHIFT) {
        button += 4;
    }
    if modifiers.contains(KeyModifiers::ALT) {
        button += 8;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        button += 16;
    }
    let (column, row) = (column.saturating_add(1), row.saturating_add(1));
    Some(match encoding.encoding {
        // `CSI < button ; col ; row M` for a press, `m` for a release. The only encoding with
        // no coordinate ceiling, which is why every modern program asks for it.
        vt100::MouseProtocolEncoding::Sgr => format!(
            "\x1b[<{button};{column};{row}{}",
            if release { 'm' } else { 'M' }
        )
        .into_bytes(),
        // X10 and its UTF-8 extension both bias by 32. A release is button 3 rather than a
        // distinct terminator, so the button is replaced rather than flagged.
        vt100::MouseProtocolEncoding::Default | vt100::MouseProtocolEncoding::Utf8 => {
            let button = if release { 3 } else { button };
            let mut bytes = b"\x1b[M".to_vec();
            for value in [
                button + 32,
                column.min(223) as u32 + 32,
                row.min(223) as u32 + 32,
            ] {
                if encoding.encoding == vt100::MouseProtocolEncoding::Utf8 && value > 127 {
                    let mut buffer = [0_u8; 4];
                    bytes.extend_from_slice(
                        char::from_u32(value)
                            .unwrap_or('\u{fffd}')
                            .encode_utf8(&mut buffer)
                            .as_bytes(),
                    );
                } else {
                    bytes.push(value.min(255) as u8);
                }
            }
            bytes
        }
    })
}

fn mouse_button(button: crossterm::event::MouseButton) -> u32 {
    match button {
        crossterm::event::MouseButton::Left => 0,
        crossterm::event::MouseButton::Middle => 1,
        crossterm::event::MouseButton::Right => 2,
    }
}
