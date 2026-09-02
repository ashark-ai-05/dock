//! A filtered, keyboard-driven chooser shared by every "pick one of these" overlay.
//!
//! Workspaces and files are different things to choose between, but choosing is the same act: type
//! to narrow, move a cursor, take the highlighted row. Keeping that act in one place means the two
//! overlays cannot drift into behaving differently under the same keys.

/// One choosable row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerItem {
    /// What selecting this row yields to the caller — a workspace id, a path.
    pub key: String,
    /// The text matched against and shown first.
    pub label: String,
    /// Dimmed trailing context. Never matched against, so a query cannot select a row by text the
    /// user cannot see themselves having typed.
    pub detail: String,
}

#[derive(Debug, Clone, Default)]
pub struct Picker {
    items: Vec<PickerItem>,
    query: String,
    /// Indices into `items`, best match first. Rebuilt on every query change.
    matches: Vec<usize>,
    selected: usize,
}

impl Picker {
    pub fn new(items: Vec<PickerItem>) -> Self {
        let mut picker = Self {
            items,
            ..Self::default()
        };
        picker.refilter();
        picker
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn is_empty(&self) -> bool {
        self.matches.is_empty()
    }

    pub fn push(&mut self, character: char) {
        self.query.push(character);
        self.refilter();
    }

    pub fn pop(&mut self) {
        self.query.pop();
        self.refilter();
    }

    /// Moves the highlight, saturating at both ends. A list that wraps makes it impossible to tell
    /// "I am at the bottom" from "I have looped", which matters when the list is longer than the
    /// overlay can show.
    pub fn move_selection(&mut self, delta: isize) {
        let Some(last) = self.matches.len().checked_sub(1) else {
            return;
        };
        self.selected = self.selected.saturating_add_signed(delta).min(last);
    }

    /// The rows to draw, best first, each flagged if it is the highlighted one.
    pub fn visible(&self) -> impl Iterator<Item = (&PickerItem, bool)> {
        self.matches
            .iter()
            .enumerate()
            .map(move |(row, &index)| (&self.items[index], row == self.selected))
    }

    pub fn selected(&self) -> Option<&PickerItem> {
        self.matches.get(self.selected).map(|&i| &self.items[i])
    }

    fn refilter(&mut self) {
        let query = self.query.to_lowercase();
        let mut scored: Vec<(u32, usize)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                score(&query, &item.label.to_lowercase()).map(|score| (score, index))
            })
            .collect();
        // Best score first; ties keep the caller's order, which is the order the user already sees
        // elsewhere (tab order for workspaces, directory order for files).
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        self.matches = scored.into_iter().map(|(_, index)| index).collect();
        self.selected = self.selected.min(self.matches.len().saturating_sub(1));
    }
}

/// How well `candidate` matches `query`, or `None` if it does not match at all.
///
/// The rule is subsequence matching: every character of the query must appear in the candidate, in
/// order, but not necessarily together — so `dsh` finds `dashboard`. Both arguments arrive already
/// lowercased, so matching is case-insensitive.
///
/// Ranking rewards the two things that make a guess feel right. Runs of adjacent matched characters
/// score highest, because a query that lands contiguously is almost always the one the user meant.
/// A match at the very start scores next, since people type the beginnings of names. Shorter
/// candidates then break ties, which keeps `api` above `api-gateway-staging` for the query `api`.
fn score(query: &str, candidate: &str) -> Option<u32> {
    if query.is_empty() {
        return Some(1);
    }
    let mut characters = candidate.char_indices();
    let mut score = 0_u32;
    let mut previous_position = None;
    for wanted in query.chars() {
        let (position, _) = characters.by_ref().find(|(_, actual)| *actual == wanted)?;
        score += match previous_position {
            Some(previous) if position == previous + 1 => 8,
            _ if position == 0 => 4,
            _ => 1,
        };
        previous_position = Some(position);
    }
    // Shorter candidates win ties, without ever letting length overturn a better match.
    Some(score * 4 + (64_u32.saturating_sub(candidate.chars().count() as u32)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(labels: &[&str]) -> Vec<PickerItem> {
        labels
            .iter()
            .map(|label| PickerItem {
                key: (*label).into(),
                label: (*label).into(),
                detail: String::new(),
            })
            .collect()
    }

    fn labels(picker: &Picker) -> Vec<String> {
        picker
            .visible()
            .map(|(item, _)| item.label.clone())
            .collect()
    }

    #[test]
    fn an_empty_query_offers_every_item_in_the_callers_order() {
        let picker = Picker::new(items(&["dock", "api", "notes"]));
        assert_eq!(labels(&picker), ["dock", "api", "notes"]);
        assert_eq!(picker.selected().unwrap().key, "dock");
    }

    #[test]
    fn a_query_matches_characters_in_order_without_needing_them_adjacent() {
        let mut picker = Picker::new(items(&["dashboard", "api", "docs"]));
        for character in "dsh".chars() {
            picker.push(character);
        }
        assert_eq!(labels(&picker), ["dashboard"]);
    }

    #[test]
    fn a_contiguous_match_outranks_a_scattered_one() {
        let mut picker = Picker::new(items(&["a-p-i-x", "api"]));
        for character in "api".chars() {
            picker.push(character);
        }
        assert_eq!(labels(&picker)[0], "api");
    }

    #[test]
    fn a_shorter_candidate_breaks_a_tie() {
        let mut picker = Picker::new(items(&["api-gateway-staging", "api"]));
        for character in "api".chars() {
            picker.push(character);
        }
        assert_eq!(labels(&picker)[0], "api");
    }

    #[test]
    fn matching_ignores_case_in_both_directions() {
        let mut picker = Picker::new(items(&["Dock"]));
        picker.push('d');
        assert_eq!(labels(&picker), ["Dock"]);
    }

    #[test]
    fn detail_text_is_never_matched_against() {
        let picker_items = vec![PickerItem {
            key: "one".into(),
            label: "one".into(),
            detail: "zzz".into(),
        }];
        let mut picker = Picker::new(picker_items);
        picker.push('z');
        assert!(picker.is_empty());
        assert!(picker.selected().is_none());
    }

    #[test]
    fn backspace_restores_what_the_narrower_query_hid() {
        let mut picker = Picker::new(items(&["dock", "api"]));
        picker.push('d');
        assert_eq!(labels(&picker), ["dock"]);
        picker.pop();
        assert_eq!(labels(&picker), ["dock", "api"]);
    }

    #[test]
    fn the_highlight_saturates_rather_than_wrapping() {
        let mut picker = Picker::new(items(&["one", "two"]));
        picker.move_selection(-1);
        assert_eq!(picker.selected().unwrap().key, "one");
        picker.move_selection(50);
        assert_eq!(picker.selected().unwrap().key, "two");
    }

    #[test]
    fn a_query_that_matches_nothing_selects_nothing() {
        let mut picker = Picker::new(items(&["dock"]));
        for character in "zzz".chars() {
            picker.push(character);
        }
        assert!(picker.is_empty());
        assert!(picker.selected().is_none());
    }

    #[test]
    fn narrowing_past_the_highlight_keeps_the_selection_in_range() {
        let mut picker = Picker::new(items(&["alpha", "beta", "gamma"]));
        picker.move_selection(2);
        assert_eq!(picker.selected().unwrap().key, "gamma");
        picker.push('a');
        picker.push('l');
        assert_eq!(picker.selected().unwrap().key, "alpha");
    }
}
