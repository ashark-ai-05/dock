use ratatui::{style::Color, widgets::BorderType};

use crate::detect::AgentState;

/// Semantic tokens rather than raw colours, so P4 can load alternative palettes as data
/// without touching any render code. No colour may be hardcoded outside this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub accent: Color,
    pub surface: Color,
    /// A surface that sits above `surface`.
    ///
    /// Chrome only — the sidebar, the overlays, the board pane, the footer — and never a
    /// terminal pane's body, where a background of Dock's choosing would fight every program
    /// that sets its own. Without this token every surface painted on the same flat ground
    /// and the whole dashboard read as one plane.
    pub panel: Color,
    pub muted: Color,
    pub border: Color,
    pub border_focused: Color,
    pub text: Color,
    /// Background for the copy-mode selection run. A background token rather than a
    /// foreground one, so highlighted text keeps whatever colour the program gave it.
    ///
    /// Chosen against two contrast floors, not by eye: the band's own edge against
    /// `surface` is 3.01:1 (WCAG's 3:1 minimum for non-text contrast, so the highlighted
    /// region is discernible as a region), and `text` on top of it is 4.64:1 (above AA, so
    /// the selected characters stay readable).
    pub selection: Color,
    pub blocked: Color,
    pub working: Color,
    pub done: Color,
    pub idle: Color,
}

impl Theme {
    /// "Warm terminal-modern": a restrained amber-and-teal accent pair over a neutral
    /// surface, with saturation reserved for agent state so attention is never ambiguous.
    pub const fn warm() -> Self {
        Self {
            accent: Color::Rgb(232, 168, 88),
            surface: Color::Rgb(18, 18, 20),
            panel: Color::Rgb(26, 26, 29),
            muted: Color::Rgb(122, 118, 112),
            border: Color::Rgb(80, 78, 74),
            border_focused: Color::Rgb(232, 168, 88),
            text: Color::Rgb(226, 222, 214),
            selection: Color::Rgb(70, 100, 124),
            blocked: Color::Rgb(226, 106, 94),
            // Separated from the accent by value within the same amber hue, which is how
            // `cool` solves the identical problem (its `working` sits 70.8 from its accent).
            // The previous Rgb(226,184,96) was 18.9 from this palette's accent — an agent
            // that was working looked exactly like ordinary chrome.
            //
            // accent 86.2 · blocked 70.7 · done 173.8 · idle 83.5 · 4.82:1 on surface, 4.47:1 on panel
            working: Color::Rgb(168, 120, 56),
            done: Color::Rgb(122, 176, 214),
            idle: Color::Rgb(108, 122, 114),
        }
    }

    /// "Graphite and cyan": a cool graphite ground with teal for structure — focus, the active
    /// tab, the keys you can press — and exactly one warm colour in the entire palette.
    ///
    /// That last part is the design. In `warm` the accent (232,168,88) and `working`
    /// (226,184,96) are nearly the same colour, and the accent is simultaneously the focused
    /// border, the active tab and every keybinding in the sidebar — so "an agent is working"
    /// competed for the same channel as "here is a key", and nothing amber could be urgent.
    /// Here rose is the only warm token there is, which makes `needs you` structurally
    /// incapable of being mistaken for chrome.
    pub const fn cool() -> Self {
        Self {
            accent: Color::Rgb(79, 209, 197),
            surface: Color::Rgb(18, 22, 26),
            panel: Color::Rgb(27, 32, 38),
            muted: Color::Rgb(124, 138, 145),
            border: Color::Rgb(70, 82, 90),
            border_focused: Color::Rgb(79, 209, 197),
            text: Color::Rgb(221, 228, 232),
            selection: Color::Rgb(58, 107, 120),
            blocked: Color::Rgb(242, 114, 107),
            working: Color::Rgb(53, 160, 153),
            done: Color::Rgb(122, 162, 247),
            idle: Color::Rgb(110, 118, 129),
        }
    }

    /// Every palette Dock ships, paired with its name.
    ///
    /// The palette assertions below loop over this rather than naming a theme, because the
    /// alternative already failed once: `the_agent_states_stay_far_apart` was written
    /// against `cool` alone, and `warm` shipped for weeks with `working` 18.9 units from
    /// `accent` against a floor of 60.
    pub const fn all() -> [(&'static str, Self); 2] {
        [("warm", Self::warm()), ("cool", Self::cool())]
    }

    /// `DOCK_THEME=warm` keeps the old palette. Not read through `Default`, for the reason
    /// `DOCK_SIDEBAR` is not read through it either: `Default for Theme` backs
    /// `Dashboard::default()`, which every test in this file builds a dashboard from, and a
    /// palette that changed underneath the whole suite whenever `DOCK_THEME` happened to be
    /// set in a shell or a CI runner would fail nothing while quietly rendering the wrong
    /// tokens. This is applied once, explicitly, on the real construction path — see
    /// `Dashboard::apply_theme_env`, called from `main.rs` beside `apply_sidebar_env`.
    pub fn from_env() -> Theme {
        match std::env::var("DOCK_THEME").ok().as_deref().map(str::trim) {
            Some("warm") => Theme::warm(),
            _ => Theme::cool(),
        }
    }

    pub const fn agent(&self, state: AgentState) -> Color {
        match state {
            AgentState::Blocked => self.blocked,
            AgentState::Working => self.working,
            AgentState::Done => self.done,
            AgentState::Idle => self.idle,
        }
    }

    pub const fn border_type() -> BorderType {
        BorderType::Rounded
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::cool()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::AgentState;

    /// Parameterised over both palettes, not just `cool`, so `warm` cannot silently drift out
    /// of compliance while every test written for the new palette watches only `cool`.
    #[test]
    fn agent_states_map_to_distinct_colours() {
        for theme in [Theme::warm(), Theme::cool()] {
            let colours = [
                theme.agent(AgentState::Blocked),
                theme.agent(AgentState::Working),
                theme.agent(AgentState::Done),
                theme.agent(AgentState::Idle),
            ];
            for (index, colour) in colours.iter().enumerate() {
                assert!(
                    !colours[index + 1..].contains(colour),
                    "state colours must be distinguishable"
                );
            }
        }
    }

    #[test]
    fn the_selection_background_is_distinct_from_every_other_token() {
        for theme in [Theme::warm(), Theme::cool()] {
            for other in [
                theme.accent,
                theme.surface,
                theme.panel,
                theme.muted,
                theme.border,
                theme.border_focused,
                theme.text,
                theme.blocked,
                theme.working,
                theme.done,
                theme.idle,
            ] {
                assert_ne!(
                    theme.selection, other,
                    "a highlight that matches another token is not a highlight"
                );
            }
        }
    }

    #[test]
    fn focused_and_unfocused_borders_differ() {
        let theme = Theme::warm();
        assert_ne!(theme.border, theme.border_focused);
        assert_eq!(Theme::border_type(), BorderType::Rounded);
    }

    /// Unfocused pane chrome has to be a line you can see, not a 1.2:1 ghost.
    /// 3:1 is the text floor; structural borders sit at 2:1 so they stay quieter than
    /// `border_focused` while remaining visible on both surfaces.
    #[test]
    fn unfocused_borders_clear_two_to_one_on_both_surfaces() {
        for theme in [Theme::warm(), Theme::cool()] {
            for (ground, surface) in [("surface", theme.surface), ("panel", theme.panel)] {
                let ratio = contrast(theme.border, surface);
                assert!(ratio >= 2.0, "border on {ground} is only {ratio:.2}:1");
            }
        }
    }

    /// Relative luminance, as WCAG defines it.
    fn luminance(colour: Color) -> f64 {
        let Color::Rgb(r, g, b) = colour else {
            panic!("every token in a Dock theme is an explicit RGB triple");
        };
        let channel = |value: u8| {
            let value = f64::from(value) / 255.0;
            if value <= 0.03928 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
    }

    fn contrast(a: Color, b: Color) -> f64 {
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    fn distance(a: Color, b: Color) -> f64 {
        let (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) = (a, b) else {
            panic!("rgb")
        };
        let square = |x: u8, y: u8| (f64::from(x) - f64::from(y)).powi(2);
        (square(ar, br) + square(ag, bg) + square(ab, bb)).sqrt()
    }

    /// Every token has to clear 3:1 against both surfaces it can be painted on, in every
    /// palette. `panel` sits above `surface`, so a colour chosen only against the ground
    /// can go marginal on chrome.
    #[test]
    fn every_token_is_legible_on_both_surfaces() {
        for (theme_name, theme) in Theme::all() {
            for (name, colour) in [
                ("text", theme.text),
                ("muted", theme.muted),
                ("accent", theme.accent),
                ("blocked", theme.blocked),
                ("working", theme.working),
                ("done", theme.done),
                ("idle", theme.idle),
            ] {
                for (ground, surface) in [("surface", theme.surface), ("panel", theme.panel)] {
                    let ratio = contrast(colour, surface);
                    assert!(
                        ratio >= 3.0,
                        "{theme_name}: {name} on {ground} is only {ratio:.2}:1"
                    );
                }
            }
        }
    }

    /// The selection band's two floors, which pull in opposite directions: brighter makes
    /// the band visible as a band and dimmer keeps the text on it readable.
    #[test]
    fn the_selection_band_clears_both_of_its_floors() {
        for (theme_name, theme) in Theme::all() {
            let band = contrast(theme.selection, theme.surface);
            assert!(band >= 3.0, "{theme_name}: band on surface is {band:.2}:1");
            let on_band = contrast(theme.text, theme.selection);
            assert!(
                on_band >= 4.5,
                "{theme_name}: text on band is {on_band:.2}:1"
            );
        }
    }

    /// The structural lines: the pane grid, the tab separator, the menu rule, the board's
    /// column rules. 3:1 is the wrong floor for these — they are not text and cannot clear
    /// it by design — but 1.32:1 is what `cool.border` measured before `a99d44a`, and at
    /// that ratio a grid of twelve panes photographs as one undifferentiated field of text.
    ///
    /// 2:1 is the line between dim and absent, and this is what holds it.
    #[test]
    fn every_structural_line_clears_two_to_one() {
        for (theme_name, theme) in Theme::all() {
            for (name, colour) in [
                ("border", theme.border),
                ("border_focused", theme.border_focused),
            ] {
                for (ground, surface) in [("surface", theme.surface), ("panel", theme.panel)] {
                    let ratio = contrast(colour, surface);
                    assert!(
                        ratio >= 2.0,
                        "{theme_name}: {name} on {ground} is only {ratio:.2}:1"
                    );
                }
            }
        }
    }

    /// Every palette Dock ships has to be reachable from one place, or a test that means
    /// "this rule holds" quietly degrades into "this rule holds for the palette somebody
    /// remembered to name".
    #[test]
    fn every_shipped_palette_is_enumerated() {
        let names: Vec<&str> = Theme::all().iter().map(|(name, _)| *name).collect();
        assert_eq!(names, vec!["warm", "cool"]);
    }

    /// The four agent states must stay far enough apart to be told apart at a glance, in
    /// every palette.
    ///
    /// Not theoretical: `working` and `idle` collided twice while `cool` was being chosen,
    /// and `warm` shipped with `working` 18.9 from its accent because this test named one
    /// palette by hand.
    #[test]
    fn the_agent_states_stay_far_apart() {
        for (theme_name, theme) in Theme::all() {
            let states = [
                ("blocked", theme.blocked),
                ("working", theme.working),
                ("done", theme.done),
                ("idle", theme.idle),
            ];
            for (index, (name, colour)) in states.iter().enumerate() {
                for (other, second) in &states[index + 1..] {
                    let apart = distance(*colour, *second);
                    assert!(
                        apart >= 60.0,
                        "{theme_name}: {name} and {other} are only {apart:.1} apart"
                    );
                }
                let from_accent = distance(*colour, theme.accent);
                assert!(
                    from_accent >= 60.0,
                    "{theme_name}: {name} is only {from_accent:.1} from the accent"
                );
            }
        }
    }
}
