/// Copy mode's selection state for one pane. Deliberately pure: it knows grid coordinates and
/// nothing about terminals, so the maths can be tested without a PTY.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopySession {
    pub run_id: String,
    cursor: (u16, u16),
    anchor: Option<(u16, u16)>,
}

impl CopySession {
    pub fn new(run_id: String, cursor: (u16, u16)) -> Self {
        Self {
            run_id,
            cursor,
            anchor: None,
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

    fn set_cursor_absolute(&mut self, row: i64, col: i64, bounds: (u16, u16)) {
        let last_row = i64::from(bounds.0.saturating_sub(1));
        let last_col = i64::from(bounds.1.saturating_sub(1));
        self.cursor = (
            u16::try_from(row.clamp(0, last_row)).unwrap_or(0),
            u16::try_from(col.clamp(0, last_col)).unwrap_or(0),
        );
    }
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
}
