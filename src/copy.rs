/// Copy mode's selection state for one pane. Deliberately pure: it knows grid coordinates and
/// nothing about terminals, so the maths can be tested without a PTY.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopySession {
    pub run_id: String,
    cursor: (u16, u16),
    anchor: Option<(u16, u16)>,
    search: Option<String>,
}

impl CopySession {
    pub fn new(run_id: String, cursor: (u16, u16)) -> Self {
        Self {
            run_id,
            cursor,
            anchor: None,
            search: None,
        }
    }

    pub fn cursor(&self) -> (u16, u16) {
        self.cursor
    }

    pub fn anchor(&self) -> Option<(u16, u16)> {
        self.anchor
    }

    pub fn selecting(&self) -> bool {
        self.anchor.is_some()
    }

    pub fn begin_selection(&mut self) {
        self.anchor = Some(self.cursor);
    }

    pub fn move_cursor(&mut self, rows: i32, cols: i32, bounds: (u16, u16)) {
        let row = i64::from(self.cursor.0) + i64::from(rows);
        let col = i64::from(self.cursor.1) + i64::from(cols);
        self.set_cursor_absolute(row, col, bounds);
    }

    pub fn set_cursor(&mut self, cursor: (u16, u16), bounds: (u16, u16)) {
        self.set_cursor_absolute(i64::from(cursor.0), i64::from(cursor.1), bounds);
    }

    /// Selection endpoints in the order they were made. Callers order them for extraction;
    /// `VtTerminal::selection_text` is order-independent.
    pub fn selection(&self) -> Option<((u16, u16), (u16, u16))> {
        self.anchor.map(|anchor| (anchor, self.cursor))
    }

    /// Moves the anchor with the viewport it was placed in.
    ///
    /// The anchor is a cell of the *visible* grid. Scrolling therefore used to leave it
    /// pointing at whatever text had moved underneath it, so a selection made and then
    /// scrolled — with the wheel, or with `k` past the top row — yanked rows the highlight had
    /// never covered, with nothing on screen to say so. Shifting it by however far the
    /// viewport moved keeps it on the characters it was put on.
    ///
    /// An anchor pushed past either edge cannot be represented in viewport coordinates at all,
    /// so the selection ends rather than clamping to a row it does not mean. That is visible:
    /// the highlight disappears, which is honest about having lost one end of the selection.
    pub fn shift_anchor(&mut self, rows: i64, bounds: (u16, u16)) {
        let Some(anchor) = self.anchor else {
            return;
        };
        let row = i64::from(anchor.0) + rows;
        if row < 0 || row >= i64::from(bounds.0) {
            self.anchor = None;
            return;
        }
        self.anchor = Some((u16::try_from(row).unwrap_or(0), anchor.1));
    }

    /// Moves the cursor with the viewport, clamping at the edges.
    ///
    /// Used for the wheel, where the whole selection should travel with the text. Keyboard
    /// motion deliberately does *not* call this: there the cursor is what the user is moving,
    /// and pinning it to the edge is what lets `k` walk into history a row at a time.
    pub fn shift_cursor(&mut self, rows: i64, bounds: (u16, u16)) {
        let row = i64::from(self.cursor.0) + rows;
        self.set_cursor_absolute(row, i64::from(self.cursor.1), bounds);
    }

    fn set_cursor_absolute(&mut self, row: i64, col: i64, bounds: (u16, u16)) {
        let last_row = i64::from(bounds.0.saturating_sub(1));
        let last_col = i64::from(bounds.1.saturating_sub(1));
        self.cursor = (
            u16::try_from(row.clamp(0, last_row)).unwrap_or(0),
            u16::try_from(col.clamp(0, last_col)).unwrap_or(0),
        );
    }

    pub fn search_query(&self) -> Option<&str> {
        self.search.as_deref()
    }

    pub fn begin_search(&mut self) {
        self.search = Some(String::new());
    }

    pub fn push_search(&mut self, character: char) {
        if let Some(query) = self.search.as_mut() {
            query.push(character);
        }
    }

    pub fn pop_search(&mut self) {
        if let Some(query) = self.search.as_mut() {
            query.pop();
        }
    }

    pub fn cancel_search(&mut self) {
        self.search = None;
    }

    /// Moves the cursor to the next or previous match, wrapping at both ends. Returns false
    /// when there is nothing to jump to, so the caller can report "no matches" rather than
    /// silently doing nothing.
    pub fn jump_to_match(
        &mut self,
        matches: &[(u16, u16)],
        forward: bool,
        bounds: (u16, u16),
    ) -> bool {
        if matches.is_empty() {
            return false;
        }
        let cursor = self.cursor;
        let target = if forward {
            matches
                .iter()
                .find(|candidate| **candidate > cursor)
                .or_else(|| matches.first())
        } else {
            matches
                .iter()
                .rev()
                .find(|candidate| **candidate < cursor)
                .or_else(|| matches.last())
        };
        if let Some(target) = target.copied() {
            self.set_cursor(target, bounds);
            return true;
        }
        false
    }
}

/// Every occurrence of `query` across the visible rows, in reading order. Case-sensitive,
/// matching what a user typing an exact string expects.
///
/// Columns are **character offsets**, not byte offsets — computed by counting `chars()` up to
/// the match, converting from `str::find`'s byte position. This is exact for ASCII and narrow
/// accented Latin (e.g. "héllo"), but it is still not true terminal cell width: CJK ideographs
/// and most emoji occupy two cells, so a row containing wide characters before a match reports
/// a column short by the count of those wide characters. Getting true cell width would need a
/// width table (e.g. `unicode-width`), which this crate does not carry. Box-drawing characters
/// (U+2500 block) are narrow in virtually every terminal font, so TUI borders never trigger
/// this; the realistic triggers are emoji status glyphs in agent output and accented paths.
pub fn find_matches(rows: &[String], query: &str) -> Vec<(u16, u16)> {
    if query.is_empty() {
        return Vec::new();
    }
    let mut matches = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        // A row index that cannot be represented as u16 could not be reported to the caller
        // (whose coordinates are u16 throughout) anyway, and no realistic scrollback or pane
        // gets anywhere near 65,536 rows, so the remaining rows are dropped rather than panic.
        let Ok(row_index) = u16::try_from(index) else {
            break;
        };
        let mut from = 0;
        while let Some(found) = row[from..].find(query) {
            let column = from + found;
            // Same rationale as the row-index guard: a column past u16::MAX cannot be
            // reported, and no realistic terminal width approaches that, so this one match
            // is skipped (not the whole row) while the scan continues past it.
            if let Ok(char_column) = u16::try_from(row[..column].chars().count()) {
                matches.push((row_index, char_column));
            }
            from = column + query.len();
        }
    }
    matches
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOUNDS: (u16, u16) = (24, 80);

    #[test]
    fn a_new_session_has_a_cursor_and_no_selection() {
        let session = CopySession::new("run".into(), (5, 10));
        assert_eq!(session.cursor(), (5, 10));
        assert_eq!(session.anchor(), None);
        assert!(!session.selecting());
        assert_eq!(session.selection(), None);
    }

    #[test]
    fn cursor_movement_is_clamped_to_the_grid() {
        let mut session = CopySession::new("run".into(), (0, 0));
        session.move_cursor(-5, -5, BOUNDS);
        assert_eq!(
            session.cursor(),
            (0, 0),
            "cannot move above or left of the grid"
        );
        session.move_cursor(9_999, 9_999, BOUNDS);
        assert_eq!(session.cursor(), (23, 79), "clamped to the last cell");
    }

    #[test]
    fn beginning_a_selection_anchors_at_the_cursor_and_tracks_movement() {
        let mut session = CopySession::new("run".into(), (2, 3));
        session.begin_selection();
        assert!(session.selecting());
        assert_eq!(session.anchor(), Some((2, 3)));
        session.move_cursor(2, 0, BOUNDS);
        assert_eq!(session.selection(), Some(((2, 3), (4, 3))));
    }

    #[test]
    fn set_cursor_drives_selection_for_a_mouse_drag() {
        let mut session = CopySession::new("run".into(), (1, 1));
        session.begin_selection();
        session.set_cursor((7, 20), BOUNDS);
        assert_eq!(session.selection(), Some(((1, 1), (7, 20))));
        session.set_cursor((9_999, 9_999), BOUNDS);
        assert_eq!(session.selection(), Some(((1, 1), (23, 79))));
    }

    fn rows() -> Vec<String> {
        vec![
            "alpha beta".to_string(),
            "gamma".to_string(),
            "beta again beta".to_string(),
        ]
    }

    #[test]
    fn find_matches_returns_every_hit_in_reading_order() {
        assert_eq!(find_matches(&rows(), "beta"), vec![(0, 6), (2, 0), (2, 11)]);
        assert_eq!(find_matches(&rows(), "nothing"), Vec::new());
        assert_eq!(
            find_matches(&rows(), ""),
            Vec::new(),
            "an empty query matches nothing"
        );
    }

    #[test]
    fn find_matches_reports_character_columns_not_byte_offsets() {
        // "é" is 2 bytes in UTF-8 but a single narrow terminal cell; the match column must
        // count characters (6), not bytes (7), or a caller positioning a cursor lands wrong.
        assert_eq!(
            find_matches(&["héllo beta".to_string()], "beta"),
            vec![(0, 6)]
        );
    }

    #[test]
    fn jumping_cycles_forward_and_backward_and_wraps() {
        let matches = find_matches(&rows(), "beta");
        let mut session = CopySession::new("run".into(), (0, 0));
        assert!(session.jump_to_match(&matches, true, BOUNDS));
        assert_eq!(session.cursor(), (0, 6));
        session.jump_to_match(&matches, true, BOUNDS);
        assert_eq!(session.cursor(), (2, 0));
        session.jump_to_match(&matches, true, BOUNDS);
        assert_eq!(session.cursor(), (2, 11));
        session.jump_to_match(&matches, true, BOUNDS);
        assert_eq!(session.cursor(), (0, 6), "wraps to the first hit");
        session.jump_to_match(&matches, false, BOUNDS);
        assert_eq!(session.cursor(), (2, 11), "wraps backward to the last hit");
    }

    #[test]
    fn jumping_with_no_matches_reports_failure_and_leaves_the_cursor_alone() {
        let mut session = CopySession::new("run".into(), (4, 4));
        assert!(!session.jump_to_match(&[], true, BOUNDS));
        assert_eq!(session.cursor(), (4, 4));
    }

    #[test]
    fn a_search_query_is_edited_and_cancelled() {
        let mut session = CopySession::new("run".into(), (0, 0));
        assert_eq!(session.search_query(), None);
        session.begin_search();
        assert_eq!(session.search_query(), Some(""));
        session.push_search('a');
        session.push_search('b');
        assert_eq!(session.search_query(), Some("ab"));
        session.pop_search();
        assert_eq!(session.search_query(), Some("a"));
        session.cancel_search();
        assert_eq!(session.search_query(), None);
    }

    #[test]
    fn clamping_survives_degenerate_bounds_and_extreme_deltas() {
        // A pane rendered with no inner area gives a zero-sized grid; the cursor must still
        // land somewhere valid rather than panicking or underflowing.
        for bounds in [(0, 0), (1, 1), (0, 80), (24, 0)] {
            let mut session = CopySession::new("run".into(), (0, 0));
            session.move_cursor(i32::MAX, i32::MAX, bounds);
            let (row, col) = session.cursor();
            assert!(
                row < bounds.0.max(1) && col < bounds.1.max(1),
                "{bounds:?} -> {row},{col}"
            );
            session.move_cursor(i32::MIN, i32::MIN, bounds);
            assert_eq!(session.cursor(), (0, 0), "{bounds:?} clamps to the origin");
        }
    }

    #[test]
    fn selection_endpoints_stay_in_creation_order_and_are_never_sorted() {
        let mut session = CopySession::new("run".into(), (5, 5));
        session.begin_selection();
        session.set_cursor((1, 1), BOUNDS);
        // Ordering is VtTerminal::selection_text's job and it is order-independent. Sorting
        // here would be redundant today and wrong if that downstream ever changed.
        assert_eq!(session.selection(), Some(((5, 5), (1, 1))));
    }
}
