use std::{
    io::Write,
    process::{Command, Stdio},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};

/// Which path actually put the text on the clipboard. Reported to the user so a silent
/// no-op is impossible — OSC 52 is disabled by default in some terminals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardRoute {
    Osc52,
    Command(&'static str),
}

/// OSC 52 asks the *host* terminal to set its clipboard, so it works over SSH where a local
/// helper binary would not.
pub fn osc52(text: &str) -> Vec<u8> {
    let mut sequence = b"\x1b]52;c;".to_vec();
    sequence.extend_from_slice(STANDARD.encode(text).as_bytes());
    sequence.push(0x07);
    sequence
}

/// Writes the selection to the system clipboard, preferring OSC 52 and falling back to a
/// platform helper. Returns which route succeeded.
pub fn copy(text: &str) -> Result<ClipboardRoute, String> {
    let mut stdout = std::io::stdout();
    if stdout.write_all(&osc52(text)).is_ok() && stdout.flush().is_ok() {
        return Ok(ClipboardRoute::Osc52);
    }
    for helper in ["pbcopy", "wl-copy", "xclip"] {
        if let Ok(mut child) = Command::new(helper)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            let wrote = child
                .stdin
                .as_mut()
                .is_some_and(|stdin| stdin.write_all(text.as_bytes()).is_ok());
            let _ = child.wait();
            if wrote {
                return Ok(ClipboardRoute::Command(helper));
            }
        }
    }
    Err("could not reach the system clipboard by OSC 52 or a helper".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;

    #[test]
    fn osc52_wraps_base64_of_the_selection() {
        let sequence = osc52("hello");
        let text = String::from_utf8(sequence).expect("osc 52 is ascii");
        assert!(text.starts_with("\x1b]52;c;"), "got {text:?}");
        assert!(text.ends_with('\x07'), "got {text:?}");
        let payload = text
            .trim_start_matches("\x1b]52;c;")
            .trim_end_matches('\x07');
        assert_eq!(STANDARD.decode(payload).expect("valid base64"), b"hello");
    }

    #[test]
    fn osc52_survives_multi_line_and_non_ascii_selections() {
        let selection = "line 1\nline 2\né 🎉";
        let text = String::from_utf8(osc52(selection)).expect("osc 52 is ascii");
        let payload = text
            .trim_start_matches("\x1b]52;c;")
            .trim_end_matches('\x07');
        let decoded = STANDARD.decode(payload).expect("valid base64");
        assert_eq!(String::from_utf8(decoded).expect("utf8"), selection);
    }

    #[test]
    fn an_empty_selection_still_produces_a_well_formed_sequence() {
        let text = String::from_utf8(osc52("")).expect("osc 52 is ascii");
        assert_eq!(text, "\x1b]52;c;\x07");
    }
}
