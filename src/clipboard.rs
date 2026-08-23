use std::{
    io::Write,
    process::{Command, Stdio},
    thread,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};

/// Which path Dock actually took to reach the clipboard. Reported to the user because a copy
/// that reached nothing looks exactly like a copy that worked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardRoute {
    Osc52,
    Command(&'static str),
}

impl ClipboardRoute {
    /// How the route reads in a notice.
    ///
    /// OSC 52 names itself as a *request* rather than a result, and that wording is the whole
    /// point of this method. The sequence is one-way — the terminal never acknowledges it —
    /// and Terminal.app disables it outright, iTerm2 disables it by default, and tmux ignores
    /// it without `set -g set-clipboard on`. The old notice said "copied N characters to the
    /// clipboard via OSC 52" on every one of those, which is a claim Dock cannot make and
    /// could not have checked.
    pub fn describe(self) -> &'static str {
        match self {
            ClipboardRoute::Osc52 => "OSC 52 (asked the terminal; it cannot acknowledge)",
            ClipboardRoute::Command("pbcopy") => "pbcopy",
            ClipboardRoute::Command("wl-copy") => "wl-copy",
            ClipboardRoute::Command("xclip") => "xclip",
            ClipboardRoute::Command(other) => other,
        }
    }
}

/// Which routes a copy should take, read from `DOCK_CLIPBOARD`.
///
/// An environment variable rather than a config file because that is how every other knob in
/// Dock is spelled (`DOCK_BOARD`, `DOCK_SOCKET_PATH`, `DOCK_TEST_TIMEOUT_SCALE`), and because
/// the answer is a property of the terminal a dashboard was started in rather than of the
/// repository it is looking at — the same checkout opened over SSH and locally wants different
/// routes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClipboardPreference {
    /// OSC 52 only. The default, because it is the only route that works over SSH.
    #[default]
    Osc52,
    /// A local helper only, for the terminals that refuse OSC 52.
    Helper,
    /// Both, for a terminal whose OSC 52 support is unknown. Setting the same text twice is
    /// harmless; the second setter simply wins.
    Both,
    /// Neither. For a user who would rather Dock never touched their clipboard.
    Off,
}

/// Parses `DOCK_CLIPBOARD`. An unrecognised value is an error rather than a fallback to the
/// default: a typo that silently keeps the route the user was trying to change is the failure
/// this variable exists to end.
pub fn preference_from(value: Option<&str>) -> Result<ClipboardPreference, String> {
    match value.map(str::trim) {
        None | Some("") | Some("auto") | Some("osc52") => Ok(ClipboardPreference::Osc52),
        Some("helper") | Some("command") => Ok(ClipboardPreference::Helper),
        Some("both") => Ok(ClipboardPreference::Both),
        Some("off") | Some("none") => Ok(ClipboardPreference::Off),
        Some(other) => Err(format!(
            "DOCK_CLIPBOARD={other:?} is not one of auto, helper, both, off"
        )),
    }
}

/// The preference in force for this process.
///
/// Pinned in test builds so the crate's own tests can never spawn `pbcopy` and overwrite the
/// clipboard of whoever is running them. The parsing above is what the tests exercise.
fn preference() -> Result<ClipboardPreference, String> {
    if cfg!(test) {
        return Ok(ClipboardPreference::Osc52);
    }
    preference_from(std::env::var("DOCK_CLIPBOARD").ok().as_deref())
}

/// OSC 52 asks the *host* terminal to set its clipboard, so it works over SSH where a local
/// helper binary would not.
pub fn osc52(text: &str) -> Vec<u8> {
    let mut sequence = b"\x1b]52;c;".to_vec();
    sequence.extend_from_slice(STANDARD.encode(text).as_bytes());
    sequence.push(0x07);
    sequence
}

/// Writes the selection to the system clipboard by every route the preference asks for, and
/// names the ones that ran.
///
/// Returns *all* the routes rather than the first that worked. The old version returned the
/// first, and since a write to a raw-mode alternate-screen stdout essentially never fails, it
/// always returned `Osc52` and the helper loop underneath it was unreachable code — which is
/// why a user on Terminal.app was told "copied" while their clipboard never changed.
pub fn copy(text: &str) -> Result<Vec<ClipboardRoute>, String> {
    copy_with(text, preference()?)
}

pub fn copy_with(
    text: &str,
    preference: ClipboardPreference,
) -> Result<Vec<ClipboardRoute>, String> {
    let mut routes = Vec::new();
    let mut failures = Vec::new();
    if matches!(
        preference,
        ClipboardPreference::Osc52 | ClipboardPreference::Both
    ) {
        match write_osc52(text) {
            Ok(()) => routes.push(ClipboardRoute::Osc52),
            Err(error) => failures.push(format!("OSC 52 write failed: {error}")),
        }
    }
    if matches!(
        preference,
        ClipboardPreference::Helper | ClipboardPreference::Both
    ) {
        match spawn_helper(text) {
            Some(helper) => routes.push(ClipboardRoute::Command(helper)),
            None => {
                failures.push("no clipboard helper on PATH (pbcopy, wl-copy, xclip)".to_owned())
            }
        }
    }
    if routes.is_empty() {
        return Err(if failures.is_empty() {
            "the clipboard is off · unset DOCK_CLIPBOARD to use it".to_owned()
        } else {
            failures.join("; ")
        });
    }
    Ok(routes)
}

/// Writes the escape sequence as one locked, flushed write.
///
/// This is a second writer to the same file descriptor ratatui's `CrosstermBackend` holds. It
/// is safe because every caller runs outside `Terminal::draw` — a yank is handled after the
/// frame is painted, never during it — and because the lock is taken for the whole sequence,
/// so a sequence can never be cut in half by another writer and land on screen as text.
fn write_osc52(text: &str) -> Result<(), String> {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    stdout
        .write_all(&osc52(text))
        .and_then(|()| stdout.flush())
        .map_err(|error| error.to_string())
}

/// Hands the text to the first helper on `PATH` and returns its name.
///
/// Two details that were wrong before and would have hung the dashboard the moment this path
/// became reachable: the child's stdin is *dropped* before anything waits on it, since a helper
/// reading to EOF never exits while the write end is still open; and the wait itself happens on
/// a detached thread, because the render loop must not block on a subprocess. The thread exists
/// only to reap the child — nothing reads its result, and there is nothing to read: these tools
/// exit 0 once they have the bytes.
fn spawn_helper(text: &str) -> Option<&'static str> {
    for helper in ["pbcopy", "wl-copy", "xclip"] {
        // `xclip` defaults to the PRIMARY selection, which is the middle-click buffer rather
        // than the clipboard Ctrl+V pastes from. Without this it "works" and pastes nothing.
        let arguments: &[&str] = if helper == "xclip" {
            &["-selection", "clipboard"]
        } else {
            &[]
        };
        let Ok(mut child) = Command::new(helper)
            .args(arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        let wrote = match child.stdin.take() {
            Some(mut stdin) => stdin
                .write_all(text.as_bytes())
                .and_then(|()| stdin.flush())
                .is_ok(),
            None => false,
        };
        thread::spawn(move || {
            let _ = child.wait();
        });
        if wrote {
            return Some(helper);
        }
    }
    None
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

    #[test]
    fn an_unset_clipboard_preference_is_osc_52_and_a_misspelt_one_is_refused() {
        assert_eq!(preference_from(None), Ok(ClipboardPreference::Osc52));
        assert_eq!(preference_from(Some("")), Ok(ClipboardPreference::Osc52));
        assert_eq!(
            preference_from(Some("auto")),
            Ok(ClipboardPreference::Osc52)
        );
        assert_eq!(
            preference_from(Some("helper")),
            Ok(ClipboardPreference::Helper)
        );
        assert_eq!(preference_from(Some("both")), Ok(ClipboardPreference::Both));
        assert_eq!(preference_from(Some("off")), Ok(ClipboardPreference::Off));
        // A typo must not quietly leave the user on the route they were trying to leave.
        let refused = preference_from(Some("pbcopy")).expect_err("an unknown value is refused");
        assert!(refused.contains("pbcopy"), "got {refused}");
        assert!(refused.contains("helper"), "the error names the valid set");
    }

    #[test]
    fn turning_the_clipboard_off_reports_why_rather_than_claiming_success() {
        let refused = copy_with("anything", ClipboardPreference::Off)
            .expect_err("an off clipboard copies nothing");
        assert!(refused.contains("off"), "got {refused}");
    }

    #[test]
    fn the_osc_52_route_describes_itself_as_a_request_the_terminal_never_answers() {
        let described = ClipboardRoute::Osc52.describe();
        assert!(described.contains("OSC 52"), "got {described}");
        assert!(
            described.contains("acknowledge"),
            "a route Dock cannot verify must say so: got {described}"
        );
        assert_eq!(ClipboardRoute::Command("pbcopy").describe(), "pbcopy");
    }
}
