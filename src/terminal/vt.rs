use std::sync::{Arc, Mutex};

/// OSC state that `vt100` does not model itself. Captured through `Callbacks` so shell
/// integration (OSC 7 working directory, OSC 133 semantic prompts) costs no extra parsing.
#[derive(Debug, Default, Clone)]
pub struct ShellSignals {
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub last_exit_status: Option<i32>,
}

#[derive(Default)]
struct TerminalHooks(Arc<Mutex<ShellSignals>>);

impl vt100::Callbacks for TerminalHooks {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        let mut signals = self.0.lock().unwrap_or_else(|p| p.into_inner());
        signals.title = Some(String::from_utf8_lossy(title).into_owned());
    }

    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        let parts: Vec<String> = params
            .iter()
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect();
        let mut signals = self.0.lock().unwrap_or_else(|p| p.into_inner());
        match parts.first().map(String::as_str) {
            // OSC 7 reports the working directory as a file:// URL.
            Some("7") => signals.cwd = parts.get(1).and_then(|url| parse_cwd(url)),
            // OSC 133;D;<code> marks the end of a command with its exit status.
            Some("133") if parts.get(1).map(String::as_str) == Some("D") => {
                signals.last_exit_status = parts.get(2).and_then(|code| code.parse().ok());
            }
            _ => {}
        }
    }
}

fn parse_cwd(url: &str) -> Option<String> {
    let rest = url.strip_prefix("file://")?;
    let path = rest.find('/').map(|index| &rest[index..])?;
    Some(path.to_owned())
}

pub struct VtTerminal {
    parser: vt100::Parser<TerminalHooks>,
    signals: Arc<Mutex<ShellSignals>>,
}

impl VtTerminal {
    pub fn new(rows: u16, cols: u16, scrollback_rows: usize) -> Self {
        let signals = Arc::new(Mutex::new(ShellSignals::default()));
        let hooks = TerminalHooks(Arc::clone(&signals));
        Self {
            parser: vt100::Parser::new_with_callbacks(rows, cols, scrollback_rows, hooks),
            signals,
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(rows, cols);
    }

    pub fn size(&self) -> (u16, u16) {
        self.parser.screen().size()
    }

    pub fn cursor(&self) -> (u16, u16) {
        self.parser.screen().cursor_position()
    }

    pub fn alternate_screen(&self) -> bool {
        self.parser.screen().alternate_screen()
    }

    /// Full screen state including cursor, sufficient to reconstruct this terminal exactly.
    pub fn state_bytes(&self) -> Vec<u8> {
        self.parser.screen().state_formatted()
    }

    /// The last `rows` lines of output ending at the cursor, not the bottom of the
    /// configured screen size — a freshly opened pane has not scrolled the cursor to the
    /// last row yet, so anchoring on screen height would return blank lines instead of
    /// the output that was actually just written.
    pub fn text_tail(&self, rows: u16) -> String {
        let (_, width) = self.size();
        let (cursor_row, _) = self.cursor();
        let end = cursor_row + 1;
        let start = end.saturating_sub(rows);
        self.parser.screen().contents_between(start, 0, end, width)
    }

    pub fn title(&self) -> Option<String> {
        self.signals
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .title
            .clone()
    }

    pub fn cwd(&self) -> Option<String> {
        self.signals
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .cwd
            .clone()
    }

    pub fn last_exit_status(&self) -> Option<i32> {
        self.signals
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .last_exit_status
    }

    pub(crate) fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::ScreenSync;

    #[test]
    fn parses_styled_output_and_reports_geometry() {
        let mut term = VtTerminal::new(24, 80, 100);
        term.feed(b"\x1b[1;32mgreen\x1b[0m plain");
        assert_eq!(term.size(), (24, 80));
        assert!(!term.alternate_screen());
        assert!(term.text_tail(1).contains("green plain"));
    }

    #[test]
    fn captures_osc_title_cwd_and_command_exit_status() {
        let mut term = VtTerminal::new(24, 80, 100);
        term.feed(b"\x1b]2;my title\x07");
        term.feed(b"\x1b]7;file://host/Users/krunal/dock\x07");
        term.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07false\x1b]133;C\x07\x1b]133;D;1\x07");
        assert_eq!(term.title().as_deref(), Some("my title"));
        assert_eq!(term.cwd().as_deref(), Some("/Users/krunal/dock"));
        assert_eq!(term.last_exit_status(), Some(1));
    }

    #[test]
    fn resize_changes_reported_size() {
        let mut term = VtTerminal::new(24, 80, 100);
        term.resize(40, 120);
        assert_eq!(term.size(), (40, 120));
    }

    #[test]
    fn sync_converges_through_sgr_alt_screen_and_resize() {
        let mut live = VtTerminal::new(24, 80, 100);
        let mut sync = ScreenSync::new(24, 80);
        let writes: Vec<&[u8]> = vec![
            b"\x1b[1;32m$ cargo build\x1b[0m\r\n",
            b"\x1b[2J\x1b[H",
            b"\x1b[?1049h",
            b"\x1b[3;10Halt-screen TUI\x1b[K",
            b"\x1b[?1049l",
            b"back to normal\r\n",
        ];
        for write in writes {
            live.feed(write);
            let delta = sync.delta_from(&live);
            sync.apply(&delta);
            assert_eq!(sync.state_bytes(), live.state_bytes());
        }
        live.resize(30, 100);
        sync.resize(30, 100);
        live.feed(b"after resize");
        let delta = sync.delta_from(&live);
        sync.apply(&delta);
        assert_eq!(sync.state_bytes(), live.state_bytes());
        assert_eq!(sync.cursor(), live.cursor());
    }

    #[test]
    fn idle_screen_produces_an_empty_delta() {
        let mut live = VtTerminal::new(24, 80, 100);
        live.feed(b"settled output\r\n");
        let mut sync = ScreenSync::new(24, 80);
        let delta = sync.delta_from(&live);
        sync.apply(&delta);
        assert!(sync.delta_from(&live).is_empty());
    }
}
