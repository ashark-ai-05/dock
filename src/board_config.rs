//! What a board says about itself, read from `kanban/config.yml`.
//!
//! Dock had reimplemented a smaller, different board beside the one the repository already
//! declares. `board::STATUSES` hardcoded `todo`, which this repository's own config does not
//! have, so every board drew a `TODO` column that existed only in Dock's source; and it had no
//! `needs-input`, which the config *does* declare, so a card could never be moved into the one
//! column an agent workflow most needs. The config also asks for two-line cards and for cards to
//! be coloured by age, and neither reached the screen.
//!
//! Deliberately not a YAML parser, for the same reason [`crate::board`]'s front-matter reader is
//! not one: the handful of keys wanted are plain scalars and lists of scalars at known
//! indentation, and everything else in the file — priorities, classes, claim timeouts, whatever
//! a later version of kanban-md adds — must be ignored rather than interpreted. A config Dock
//! cannot fully parse is still a config Dock can read the statuses out of.

use std::path::Path;

/// One rung of the age colouring: after this long without an update, a card takes this colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgeThreshold {
    /// Seconds since the card was last touched.
    pub after: u64,
    /// The 256-colour index the board declares for this rung.
    pub colour: u8,
}

/// The parts of `config.yml` Dock renders from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardConfig {
    /// The columns this board actually has, in the order the file lists them.
    pub statuses: Vec<String>,
    /// How many lines a card's title may take. `tui.title_lines`.
    pub title_lines: usize,
    /// Age rungs, oldest last. Empty when the board declares none.
    pub age_thresholds: Vec<AgeThreshold>,
}

impl Default for BoardConfig {
    /// What a board with no `config.yml` of its own gets: the shape kanban-md gives every board
    /// it creates.
    ///
    /// This used to be Dock's own answer — `todo` as a column, one-line cards, no age colouring
    /// — and that answer only ever agreed with a board that had a config to override it. Every
    /// *personal* board has none, so a workspace board drew a `TODO` column that exists in no
    /// kanban-md config, cut every title on one line, and coloured nothing by age. Reading the
    /// config fixed repository boards and left personal ones exactly as they were.
    ///
    /// The rungs are kanban-md's own defaults: grey while fresh, green after an hour, yellow
    /// after a day, orange after three, red after a week.
    fn default() -> Self {
        Self {
            statuses: crate::board::STATUSES
                .iter()
                .map(|status| (*status).to_owned())
                .collect(),
            title_lines: 2,
            age_thresholds: vec![
                AgeThreshold {
                    after: 0,
                    colour: 242,
                },
                AgeThreshold {
                    after: 3_600,
                    colour: 34,
                },
                AgeThreshold {
                    after: 86_400,
                    colour: 226,
                },
                AgeThreshold {
                    after: 259_200,
                    colour: 208,
                },
                AgeThreshold {
                    after: 604_800,
                    colour: 196,
                },
            ],
        }
    }
}

/// The columns kanban-md puts on a board it creates.
///
/// Note what is *not* here: `todo`. No kanban-md config declares it, and Dock's own constant did
/// — so every board without a config drew an empty column that exists nowhere else. `needs-input`
/// is here instead, which is where an agent parks work it cannot finish without you.
pub const KANBAN_MD_STATUSES: [&str; 5] =
    ["backlog", "in-progress", "needs-input", "review", "done"];

/// Reads the config that governs `tasks_dir`, which sits beside it.
///
/// Takes the *tasks* directory because that is what every caller already holds; the config is
/// its parent's `config.yml`. Returns the default for a board with no config, an unreadable
/// one, or one whose `statuses` list is empty — a board with no columns is not a board, and
/// falling back is better than rendering nothing.
pub fn load(tasks_dir: &Path) -> BoardConfig {
    let Some(root) = tasks_dir.parent() else {
        return BoardConfig::default();
    };
    let Ok(text) = std::fs::read_to_string(root.join("config.yml")) else {
        return BoardConfig::default();
    };
    parse(&text)
}

/// The indentation a line carries, in spaces. Tabs are not YAML indentation and are counted as
/// nothing, which keeps a tab-indented file from silently reading as top-level keys.
fn indent(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

fn unquote(value: &str) -> &str {
    let value = value.trim();
    value
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|rest| rest.strip_suffix('\''))
        })
        .unwrap_or(value)
}

/// `30s`, `2h`, `168h`, or a bare number of seconds. Anything else is `None`, which drops that
/// rung rather than the whole file.
fn duration_seconds(value: &str) -> Option<u64> {
    let value = unquote(value);
    let (digits, multiplier) = match value.chars().last()? {
        's' => (&value[..value.len() - 1], 1),
        'm' => (&value[..value.len() - 1], 60),
        'h' => (&value[..value.len() - 1], 3_600),
        'd' => (&value[..value.len() - 1], 86_400),
        _ => (value, 1),
    };
    digits.trim().parse::<u64>().ok().map(|n| n * multiplier)
}

pub fn parse(text: &str) -> BoardConfig {
    let mut config = BoardConfig::default();
    let mut statuses = Vec::new();
    // Collected apart from the defaults and swapped in whole, exactly as the statuses are: a
    // board that declares its own rungs means *these* rungs, not these on top of ours.
    let mut rungs: Vec<AgeThreshold> = Vec::new();
    // Which top-level block the scanner is inside. A block ends at the next line whose
    // indentation returns to zero, which is what keeps `statuses:` from swallowing `tui:`.
    let mut section: Option<&str> = None;
    let mut in_age = false;
    let mut pending_after: Option<u64> = None;

    for line in text.lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        if indent(line) == 0 {
            section = line.split_once(':').map(|(key, _)| key.trim());
            in_age = false;
            pending_after = None;
            continue;
        }
        let trimmed = line.trim();
        match section {
            // `- name: backlog`. Only the name is wanted; a status may carry a wip limit or a
            // description and those belong to kanban-md, not to Dock's rendering.
            Some("statuses") => {
                if let Some(rest) = trimmed.strip_prefix("- ")
                    && let Some((key, value)) = rest.split_once(':')
                    && key.trim() == "name"
                {
                    statuses.push(unquote(value).to_owned());
                }
            }
            Some("tui") => {
                if let Some((key, value)) = trimmed.split_once(':') {
                    match key.trim().trim_start_matches("- ") {
                        "title_lines" => {
                            if let Ok(lines) = unquote(value).parse::<usize>() {
                                // A card is a card, not a paragraph. Clamped so a config asking
                                // for twenty lines cannot turn one card into a whole column.
                                config.title_lines = lines.clamp(1, 4);
                            }
                        }
                        "age_thresholds" => in_age = true,
                        // A rung is two lines: `- after: 24h` then `color: "226"`. The `after`
                        // is held until its colour arrives, so a malformed pair drops itself
                        // rather than pairing with the next rung's colour.
                        "after" if in_age => pending_after = duration_seconds(value),
                        "color" | "colour" if in_age => {
                            if let (Some(after), Ok(colour)) =
                                (pending_after.take(), unquote(value).parse::<u8>())
                            {
                                rungs.push(AgeThreshold { after, colour });
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    if !statuses.is_empty() {
        config.statuses = statuses;
    }
    if !rungs.is_empty() {
        config.age_thresholds = rungs;
    }
    config.age_thresholds.sort_by_key(|rung| rung.after);
    config
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This repository's own `kanban/config.yml`, which is the file that motivated all of this.
    const REAL: &str = r#"version: 11
board:
    name: Dock
tasks_dir: tasks
statuses:
    - name: backlog
    - name: in-progress
    - name: needs-input
    - name: review
    - name: done
priorities:
    - low
    - medium
    - high
    - critical
defaults:
    status: backlog
    priority: medium
    class: standard
claim_timeout: 1h
classes:
    - name: expedite
      wip_limit: 1
      bypass_column_wip: true
    - name: standard
tui:
    title_lines: 2
    age_thresholds:
        - after: 0s
          color: "242"
        - after: 1h
          color: "34"
        - after: 24h
          color: "226"
        - after: 72h
          color: "208"
        - after: 168h
          color: "196"
next_id: 11
"#;

    /// The whole point: the columns come from the board, not from a constant in Dock.
    #[test]
    fn the_columns_are_the_ones_the_board_declares() {
        let config = parse(REAL);
        assert_eq!(
            config.statuses,
            ["backlog", "in-progress", "needs-input", "review", "done"],
            "in the file's own order"
        );
        assert!(
            !config.statuses.iter().any(|status| status == "todo"),
            "`todo` is Dock's invention and must not appear: {:?}",
            config.statuses
        );
        assert!(
            config.statuses.iter().any(|status| status == "needs-input"),
            "and `needs-input` is real and must: {:?}",
            config.statuses
        );
    }

    #[test]
    fn the_card_shape_and_age_rungs_are_read_too() {
        let config = parse(REAL);
        assert_eq!(config.title_lines, 2, "the board asks for two-line cards");
        assert_eq!(
            config.age_thresholds,
            [
                AgeThreshold {
                    after: 0,
                    colour: 242
                },
                AgeThreshold {
                    after: 3_600,
                    colour: 34
                },
                AgeThreshold {
                    after: 86_400,
                    colour: 226
                },
                AgeThreshold {
                    after: 259_200,
                    colour: 208
                },
                AgeThreshold {
                    after: 604_800,
                    colour: 196
                },
            ],
            "every rung, in ascending age"
        );
    }

    /// Keys Dock does not render from must not break the keys it does. `priorities`, `classes`
    /// and `claim_timeout` all sit between `statuses` and `tui` in the real file.
    #[test]
    fn unknown_sections_are_ignored_rather_than_interpreted() {
        let config = parse(REAL);
        assert_eq!(config.statuses.len(), 5, "no stray entry from `priorities`");
        assert_eq!(config.title_lines, 2, "`classes` did not end the scan");
    }

    /// A board with no config of its own gets the shape kanban-md gives every board it makes,
    /// which is what a personal board — which never has one — must look like.
    #[test]
    fn a_board_with_no_config_keeps_docks_own_answer() {
        let config = parse("");
        assert_eq!(config.statuses, KANBAN_MD_STATUSES);
        assert_eq!(config.title_lines, 2);
        assert_eq!(config.age_thresholds.len(), 5);
    }

    /// A half-written rung drops itself rather than pairing with the next rung's colour, which
    /// would silently shift every threshold up by one.
    #[test]
    fn a_rung_with_no_colour_drops_itself() {
        let config = parse(
            "tui:\n    age_thresholds:\n        - after: 1h\n        - after: 24h\n          color: \"226\"\n",
        );
        assert_eq!(
            config.age_thresholds,
            [AgeThreshold {
                after: 86_400,
                colour: 226
            }],
            "the orphaned `after` is gone, not merged: {:?}",
            config.age_thresholds
        );
    }

    #[test]
    fn durations_are_read_in_the_units_the_file_uses() {
        assert_eq!(duration_seconds("0s"), Some(0));
        assert_eq!(duration_seconds("90m"), Some(5_400));
        assert_eq!(duration_seconds("168h"), Some(604_800));
        assert_eq!(duration_seconds("7d"), Some(604_800));
        assert_eq!(duration_seconds("42"), Some(42));
        assert_eq!(duration_seconds("soon"), None);
    }
}
