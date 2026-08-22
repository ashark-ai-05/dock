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

    /// Whether the program in this pane asked for bracketed paste (DECSET 2004). A paste must
    /// only be wrapped when the receiving application enabled the mode; wrapping unconditionally
    /// would type the delimiters into a program that reads them as literal input.
    pub fn bracketed_paste(&self) -> bool {
        self.parser.screen().bracketed_paste()
    }

    /// Full screen state including cursor, sufficient to reconstruct this terminal exactly.
    pub fn state_bytes(&self) -> Vec<u8> {
        self.parser.screen().state_formatted()
    }

    /// The last `rows` lines of output that actually contain content, not the bottom of
    /// the configured screen size and not necessarily the cursor's row. Two situations
    /// make a naive anchor wrong: a freshly opened pane has not scrolled the cursor to
    /// the last row yet (anchoring on screen height returns blank lines), and a write
    /// ending in `\r\n` leaves the cursor on the next, still-blank row (anchoring on the
    /// cursor returns a blank tail). So walk up from the cursor past any trailing blank
    /// rows to the last row with real content, then take `rows` lines ending there.
    pub fn text_tail(&self, rows: u16) -> String {
        let (_, width) = self.size();
        let (cursor_row, _) = self.cursor();
        let screen = self.parser.screen();
        let is_blank = |row: u16| {
            screen
                .contents_between(row, 0, row + 1, width)
                .trim()
                .is_empty()
        };

        let mut end_row = cursor_row;
        while is_blank(end_row) && end_row > 0 {
            end_row -= 1;
        }
        if is_blank(end_row) {
            // Nothing has been written to this screen at all.
            return String::new();
        }

        let end = end_row + 1;
        let start = end.saturating_sub(rows);
        screen.contents_between(start, 0, end, width)
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

    /// Moves the viewport through retained scrollback. Positive goes back into history.
    ///
    /// `vt100` clamps to the rows actually retained and, while the offset is non-zero, adjusts it
    /// as new output arrives so the visible rows stay put. That is why callers must treat the
    /// offset as opaque: only "zero versus non-zero" is meaningful.
    pub fn scroll_by(&mut self, delta: i32) {
        let current = i64::try_from(self.parser.screen().scrollback()).unwrap_or(i64::MAX);
        let target = (current + i64::from(delta)).max(0);
        let target = usize::try_from(target).unwrap_or(usize::MAX);
        self.parser.screen_mut().set_scrollback(target);
    }

    pub fn scroll_offset(&self) -> usize {
        self.parser.screen().scrollback()
    }

    pub fn scroll_to_live(&mut self) {
        self.parser.screen_mut().set_scrollback(0);
    }

    pub fn is_scrolled(&self) -> bool {
        self.scroll_offset() != 0
    }

    pub fn visible_row(&self, row: u16) -> String {
        let (_, cols) = self.size();
        self.parser.screen().contents_between(row, 0, row, cols)
    }

    /// Text between two points of the visible grid, in reading order regardless of which
    /// point was anchored first.
    pub fn selection_text(&self, from: (u16, u16), to: (u16, u16)) -> String {
        let (start, end) = if (from.0, from.1) <= (to.0, to.1) {
            (from, to)
        } else {
            (to, from)
        };
        self.parser
            .screen()
            .contents_between(start.0, start.1, end.0, end.1)
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
    fn text_tail_skips_the_blank_row_left_by_a_trailing_newline() {
        let mut term = VtTerminal::new(24, 80, 100);
        term.feed(b"hello\r\n");
        // The cursor now sits on row 1, which is still blank; the tail must walk back
        // up to row 0, which actually holds "hello".
        assert_eq!(term.cursor().0, 1);
        assert_eq!(term.text_tail(1).trim(), "hello");
    }

    #[test]
    fn text_tail_finds_recent_content_after_scrolling_past_screen_height() {
        let mut term = VtTerminal::new(24, 80, 100);
        for line in 0..40 {
            term.feed(format!("line{line}\r\n").as_bytes());
        }
        let tail = term.text_tail(3);
        assert!(tail.contains("line39"));
        assert!(!tail.contains("line36"));
    }

    #[test]
    fn text_tail_on_an_entirely_blank_screen_is_empty_and_does_not_panic() {
        let term = VtTerminal::new(24, 80, 100);
        assert!(term.text_tail(1).trim().is_empty());
        assert!(term.text_tail(24).trim().is_empty());
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

    fn filled(rows: u16, cols: u16, lines: usize) -> VtTerminal {
        let mut term = VtTerminal::new(rows, cols, 100);
        for index in 1..=lines {
            term.feed(format!("line {index}\r\n").as_bytes());
        }
        term
    }

    #[test]
    fn scrolling_back_shows_older_rows_and_returning_to_live_restores_the_tail() {
        let mut term = filled(5, 40, 20);
        assert!(!term.is_scrolled());
        term.scroll_by(10);
        assert!(term.is_scrolled());
        assert_eq!(term.visible_row(0).trim(), "line 7");
        term.scroll_to_live();
        assert!(!term.is_scrolled());
        assert_eq!(term.scroll_offset(), 0);
    }

    #[test]
    fn the_viewport_is_pinned_while_scrolled_even_as_new_output_arrives() {
        let mut term = filled(5, 40, 20);
        term.scroll_by(10);
        let before = term.visible_row(0);
        term.feed(b"NEW OUTPUT\r\n");
        // vt100 auto-adjusts the offset to hold the view still, so assert on the ROW, never on
        // the offset number, which legitimately changes.
        assert_eq!(term.visible_row(0), before);
        assert!(term.is_scrolled());
    }

    #[test]
    fn scrolling_is_clamped_at_both_ends() {
        let mut term = filled(5, 40, 20);
        term.scroll_by(9_999);
        let top = term.visible_row(0);
        term.scroll_by(9_999);
        assert_eq!(term.visible_row(0), top, "already at the oldest row");
        term.scroll_by(-9_999);
        assert_eq!(term.scroll_offset(), 0);
        term.scroll_by(-9_999);
        assert_eq!(term.scroll_offset(), 0, "cannot scroll past live output");
    }

    // CONTROLLER RULING C6: the full-row-aligned test below cannot tell a reading-order run
    // from a coordinate-wise rectangle, because at columns 0 and `width - 1` the two agree.
    // Mouse drag selection makes mid-row endpoints the normal case, so pin the difference.
    #[test]
    fn a_mid_row_selection_is_a_reading_order_run_not_a_rectangle() {
        let mut term = filled(5, 40, 20);
        term.scroll_by(10);
        // Rows 0..=2 hold "line 7", "line 8", "line 9". A coordinate-wise rectangle would
        // return columns 3..5 of each row ("e 7" / "e 8" / "e 9"); a reading-order run
        // returns the tail of row 0, all of row 1, and the head of row 2 — which is what a
        // user dragging from mid-line to mid-line means.
        let selected = term.selection_text((0, 5), (2, 3));
        assert_eq!(selected, "7\nline 8\nlin", "{selected:?}");
        assert_eq!(
            selected,
            term.selection_text((2, 3), (0, 5)),
            "order-independent"
        );
    }

    #[test]
    fn selection_text_is_order_independent_and_spans_rows() {
        let mut term = filled(5, 40, 20);
        term.scroll_by(10);
        let forward = term.selection_text((0, 0), (2, 39));
        let reversed = term.selection_text((2, 39), (0, 0));
        assert_eq!(forward, reversed);
        assert!(forward.contains("line 7"));
        assert!(forward.contains("line 9"));
    }
}
