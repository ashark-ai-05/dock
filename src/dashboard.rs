use std::collections::HashMap;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    buffer::{Buffer, Cell},
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use tui_term::widget::{Cursor, PseudoTerminal};

use crate::{
    adapter::{AdapterId, AdapterSelection},
    clipboard::{self, ClipboardRoute},
    copy::{CopySession, find_matches},
    detect::{AgentKind, AgentState},
    discovery::ExternalAgentCandidate,
    keymap::{FocusDirection, KeyOutcome, Keymap, PaneCommand},
    layout::{LayoutNode, LayoutSnapshot, PaneLayout, PaneRuntime, SplitAxis, WorkspaceLayout},
    protocol::{
        BindingKind, DashboardProfile, DispatchRequest, Event, LaunchIntoPaneRequest,
        PROTOCOL_VERSION, Request, RuntimeSnapshot, TerminalLaunchRequest, WorkspaceRequest,
    },
    terminal::{KeyEncoding, PaneScreen, encode_paste},
    theme::Theme,
};

/// Copy mode's bindings, published in the footer for as long as the mode is active. It is
/// the only way in without reading the help, and the only reminder of the way out.
const COPY_HINTS: &str =
    "hjkl move \u{b7} v select \u{b7} y yank \u{b7} / search \u{b7} n/N next/prev \u{b7} Esc exit";

const MIN_PANE_WIDTH: u16 = 8;
const MIN_PANE_HEIGHT: u16 = 3;

/// Scrollback capacity for this client's own `VtTerminal` replica of each pane, seeded on
/// `PaneAttached`. Mirrors `dockd`'s own `--scrollback-rows` default (see `src/bin/dockd.rs`),
/// but the attach frame carries no capacity field, so a daemon started with a non-default
/// `--scrollback-rows` desyncs silently from this constant: the client can never retain more
/// history than this, even if the daemon retains more (or less).
const DEFAULT_CLIENT_SCROLLBACK_ROWS: usize = 2000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiCommand {
    Request(Box<Request>),
    /// Raw bytes bound for the focused pane's PTY. Kept apart from `Request` because the render
    /// loop must send it without waiting for a reply: the echo comes back on the event stream,
    /// so blocking here would put a daemon round trip in front of every keystroke's paint.
    PaneInput(Vec<u8>),
    LoadCatalog,
    Refresh,
    Quit,
    None,
}

#[derive(Default)]
pub struct Dashboard {
    pub layout: LayoutSnapshot,
    pub runs: Vec<RuntimeSnapshot>,
    pub external: Vec<ExternalAgentCandidate>,
    pub repository_root: String,
    pub runtime_directory: String,
    pub repository_launches: Vec<RepositoryLaunchOption>,
    pub workspace_index: usize,
    pub error: Option<String>,
    /// This client's own emulator for each run, advanced by pushed deltas. The daemon holds the
    /// authoritative screen; this is the local replica the dashboard actually paints from.
    pub screens: HashMap<String, PaneScreen>,
    /// Latest agent identity and state per run, as pushed by the daemon.
    pub agents: HashMap<String, (Option<AgentKind>, AgentState)>,
    revisions: HashMap<String, u64>,
    needs_refresh: bool,
    pending_resizes: Vec<(String, String, u16, u16)>,
    /// The run and inner geometry last announced for each pane, so an unchanged frame
    /// announces nothing.
    pane_geometry: HashMap<String, (String, u16, u16)>,
    keymap: Keymap,
    theme: Theme,
    /// Pane rendered alone at full size, if any. Purely local: the daemon's layout tree is
    /// untouched, so zooming costs no request.
    zoomed: Option<String>,
    pane_areas: HashMap<String, Rect>,
    /// The body of each pane, inside its border. Kept alongside `pane_areas` because the
    /// pointer-to-grid conversion for drag selection has to skip the border cells, and only
    /// the render pass knows where the border ended up.
    pane_inner_areas: HashMap<String, Rect>,
    dividers: Vec<Divider>,
    dragging: Option<DragTarget>,
    /// A left button held down inside a pane body, and the cell it went down on. Distinct
    /// from `dragging`, which is the divider gesture; a press lands on one or the other.
    pane_drag: Option<PaneDrag>,
    sequence: u64,
    dismiss_external_area: Option<Rect>,
    launch_area: Option<Rect>,
    launch_form: Option<LaunchForm>,
    launch_profile_areas: Vec<Rect>,
    launch_confirm_area: Option<Rect>,
    launch_mode_area: Option<Rect>,
    help_open: bool,
    /// Copy mode's session, if active. Client-local: reading history costs the daemon nothing.
    copy: Option<CopySession>,
    /// True only while copy mode's `/` prompt is taking characters. Kept beside `copy` rather
    /// than inside `CopySession` because the query outlives the prompt: `n`/`N` reuse it once
    /// Enter has closed the editor.
    copy_searching: bool,
    rename_form: Option<String>,
    last_launch_profile: usize,
    last_repository_mode: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryLaunchOption {
    pub task_ref: String,
    pub worktree: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LaunchForm {
    index: usize,
    repository_mode: bool,
    confirming: bool,
    query: String,
}

const PROFILES: &[(DashboardProfile, &str)] = &[
    (DashboardProfile::Fixture, "Fixture"),
    (DashboardProfile::Amp, "Amp"),
    (DashboardProfile::ClaudeCode, "Claude Code"),
    (DashboardProfile::CodexCli, "Codex CLI"),
    (DashboardProfile::GithubCopilotCli, "GitHub Copilot CLI"),
];

#[derive(Debug, Clone)]
struct Divider {
    area: Rect,
    pane_id: String,
    axis: SplitAxis,
    container: Rect,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DragTarget {
    pane_id: String,
    axis: SplitAxis,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PaneDrag {
    run_id: String,
    /// Grid cell the press landed on, which becomes the selection anchor.
    origin: (u16, u16),
    /// The pane body at press time, so the pointer can be mapped to cells for the rest of
    /// the gesture even if a later frame moves the pane.
    inner: Rect,
}

impl Dashboard {
    pub fn set_repository_catalog(
        &mut self,
        repository_root: String,
        repository_launches: Vec<RepositoryLaunchOption>,
    ) {
        self.repository_root = repository_root;
        self.repository_launches = repository_launches;
        if self.repository_launches.is_empty()
            && let Some(form) = self.launch_form.as_mut()
        {
            form.repository_mode = false;
        }
    }

    /// Feeds a pushed event into this client's own emulator.
    ///
    /// `PaneAttached` is always a (re-)seed: the daemon sends it when a run is first seen and
    /// again whenever the pane's geometry changes, so the parser is rebuilt at the announced
    /// `rows`/`cols` rather than reused. Keeping the old parser would silently render the
    /// snapshot at the wrong width.
    ///
    /// A non-contiguous revision means this client missed bytes, so the screen is dropped
    /// rather than advanced into a corrupted grid.
    pub fn apply_event(&mut self, event: Event) {
        match event {
            Event::PaneAttached {
                run_id,
                revision,
                rows,
                cols,
                screen,
            } => {
                // A zero capacity here would leave `vt100` unable to retain any scrolled-off
                // rows at all (that capacity is fixed for the terminal's lifetime), so the wheel
                // (`Dashboard::mouse`'s `ScrollUp`/`ScrollDown` arm) would have nothing to scroll
                // into no matter how much output the pane produced.
                let mut terminal = PaneScreen::new(rows, cols, DEFAULT_CLIENT_SCROLLBACK_ROWS);
                if let Ok(bytes) = STANDARD.decode(&screen) {
                    terminal.feed(&bytes);
                }
                self.screens.insert(run_id.clone(), terminal);
                self.revisions.insert(run_id, revision);
            }
            Event::PaneDelta {
                run_id,
                revision,
                bytes,
            } => {
                let expected = self.revisions.get(&run_id).map(|value| value + 1);
                if expected != Some(revision) {
                    self.screens.remove(&run_id);
                    self.revisions.remove(&run_id);
                    return;
                }
                if let (Some(terminal), Ok(decoded)) =
                    (self.screens.get_mut(&run_id), STANDARD.decode(&bytes))
                {
                    terminal.feed(&decoded);
                    self.revisions.insert(run_id, revision);
                }
            }
            Event::AgentStateChanged {
                run_id,
                agent,
                state,
            } => {
                self.agents.insert(run_id, (agent, state));
            }
            Event::PaneState { .. } | Event::LayoutChanged => self.needs_refresh = true,
        }
    }

    /// Drops every replicated screen, for use when the event stream is re-established. The
    /// fresh subscription re-attaches every live run with a full snapshot, so anything not
    /// re-attached belongs to a run that is gone and would otherwise be painted forever.
    pub fn detach_screens(&mut self) {
        self.screens.clear();
        self.revisions.clear();
        // The agent roster is replicated state exactly like the screens are, and it is pushed
        // only when a run's identity or state *changes*. Left behind, every entry from before
        // the drop would keep painting a sidebar row for a run that may no longer exist.
        self.agents.clear();
    }

    /// Replaces the run list and drops any agent roster entry whose run is gone.
    ///
    /// `agents` is fed by pushed `AgentStateChanged` events, which are sent on change and never
    /// on disappearance: nothing else would ever remove an entry, so a session accumulated a
    /// dead row for every pane shell it ever retired. The authoritative run list is the only
    /// thing that knows a run has stopped existing, so pruning happens exactly where it lands.
    pub fn set_runs(&mut self, runs: Vec<RuntimeSnapshot>) {
        self.agents
            .retain(|run_id, _| runs.iter().any(|run| &run.run_id == run_id));
        self.runs = runs;
    }

    /// True once when a pushed event invalidated the run list or layout. The render loop uses
    /// this instead of an unconditional timer poll, so an idle dashboard issues no requests.
    pub fn take_refresh(&mut self) -> bool {
        std::mem::take(&mut self.needs_refresh)
    }

    /// Pane geometry changes the render pass discovered, as `(workspace_id, pane_id, rows, cols)`.
    /// Only genuinely changed geometry lands here: a resize per pane per frame would be a
    /// request storm on an otherwise idle socket.
    pub fn take_pending_resizes(&mut self) -> Vec<(String, String, u16, u16)> {
        std::mem::take(&mut self.pending_resizes)
    }

    /// True while `Ctrl+B` has been pressed and the dashboard is waiting for the command key.
    pub fn prefix_pending(&self) -> bool {
        self.keymap.is_pending()
    }

    /// The visible text of a run's replicated screen, sized from the parser's own geometry so a
    /// re-attach at a smaller pane does not read rows that no longer exist.
    pub fn screen_text(&self, run_id: &str) -> Option<String> {
        self.screens
            .get(run_id)
            .map(|screen| screen.text_tail(screen.size().0))
    }

    pub fn workspace(&self) -> Option<&WorkspaceLayout> {
        self.layout.workspaces.get(self.workspace_index)
    }

    pub fn render(&mut self, frame: &mut Frame) {
        self.pane_areas.clear();
        self.pane_inner_areas.clear();
        self.dividers.clear();
        self.dismiss_external_area = None;
        self.launch_area = None;
        self.launch_profile_areas.clear();
        self.launch_confirm_area = None;
        self.launch_mode_area = None;
        let area = frame.area();
        // Painted first so every widget that leaves cells untouched still sits on the theme's
        // surface rather than whatever the host terminal happens to use.
        frame.render_widget(
            Block::default().style(Style::default().bg(self.theme.surface).fg(self.theme.text)),
            area,
        );
        if area.width < 52 || area.height < 14 {
            self.dragging = None;
            self.render_narrow(frame, area);
            return;
        }
        let header = Rect::new(area.x, area.y, area.width, 2);
        let footer = Rect::new(area.x, area.bottom().saturating_sub(2), area.width, 2);
        let body = Rect::new(
            area.x,
            area.y + 2,
            area.width,
            area.height.saturating_sub(4),
        );
        let sidebar_width = body.width.min(28);
        let sidebar = Rect::new(body.x, body.y, sidebar_width, body.height);
        let panes = Rect::new(
            body.x + sidebar_width,
            body.y,
            body.width - sidebar_width,
            body.height,
        );
        self.render_header(frame, header);
        self.render_sidebar(frame, sidebar);
        if let Some(workspace) = self.workspace().cloned() {
            let zoomed = self
                .zoomed
                .clone()
                .filter(|pane_id| workspace.panes.contains_key(pane_id));
            match zoomed {
                Some(pane_id) => {
                    self.render_node(frame, panes, &workspace, &LayoutNode::Pane { pane_id })
                }
                None => self.render_node(frame, panes, &workspace, &workspace.root),
            }
        } else {
            frame.render_widget(
                Paragraph::new("No workspace yet. Press Ctrl+B n to create one.")
                    .style(Style::default().fg(self.theme.muted))
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_type(Theme::border_type())
                            .border_style(Style::default().fg(self.theme.border))
                            .title(" RUNTIME "),
                    ),
                panes,
            );
        }
        if self.dragging.as_ref().is_some_and(|target| {
            !self
                .dividers
                .iter()
                .any(|divider| divider.pane_id == target.pane_id && divider.axis == target.axis)
        }) {
            self.dragging = None;
        }
        if self.launch_form.is_some() {
            self.render_launch_form(frame, area);
        }
        if self.help_open {
            self.render_help(frame, area);
        }
        if self.rename_form.is_some() {
            self.render_rename(frame, area);
        }
        frame.render_widget(
            Paragraph::new(self.footer_line()).wrap(Wrap { trim: true }),
            footer,
        );
    }

    /// The single footer line. While the prefix is pending it becomes a which-key bar, which
    /// is the only place the binding table is ever published without the user asking for help.
    fn footer_line(&self) -> Line<'static> {
        // Copy mode outranks the standing hints and carries any notice alongside itself
        // rather than being replaced by one: a modal mode that goes invisible the moment
        // something goes wrong is the failure P0 deleted the last input mode over.
        if let Some(status) = self.copy_status() {
            let mut spans = vec![
                Span::styled(
                    status,
                    Style::default()
                        .fg(self.theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" \u{b7} "),
            ];
            spans.push(match self.error.as_deref() {
                Some(error) => {
                    Span::styled(error.to_owned(), Style::default().fg(self.theme.blocked))
                }
                None => Span::styled(COPY_HINTS, Style::default().fg(self.theme.muted)),
            });
            return Line::from(spans);
        }
        if let Some(error) = self.error.as_deref() {
            return Line::styled(error.to_owned(), Style::default().fg(self.theme.blocked));
        }
        if self.keymap.is_pending() {
            return Line::from(
                Keymap::hints()
                    .iter()
                    .flat_map(|(key, action)| {
                        [
                            Span::styled(
                                format!(" {key} "),
                                Style::default()
                                    .fg(self.theme.accent)
                                    .add_modifier(Modifier::BOLD),
                            ),
                            // No trailing space: the key span already opens with one, and the
                            // doubled gap cost more columns than the two-row footer has.
                            Span::styled(action.to_owned(), Style::default().fg(self.theme.text)),
                        ]
                    })
                    .collect::<Vec<_>>(),
            );
        }
        let summary = if self.help_open {
            "HELP · Esc or ? closes"
        } else if self.rename_form.is_some() {
            "RENAME · type a pane name · Enter saves · Esc cancels"
        } else if self.launch_form.is_some() {
            "LAUNCH · type to filter · Enter reviews · Esc cancels"
        } else {
            "keys go to the focused pane · Ctrl+B ? help"
        };
        Line::styled(summary, Style::default().fg(self.theme.muted))
    }

    fn render_header(&self, frame: &mut Frame, area: Rect) {
        let workspace = self.workspace().map(|w| w.name.as_str()).unwrap_or("empty");
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    " d·ock ",
                    Style::default()
                        .fg(self.theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" runtime · {workspace} · protocol v{PROTOCOL_VERSION}"),
                    Style::default().fg(self.theme.text),
                ),
            ]))
            .block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .border_style(Style::default().fg(self.theme.border)),
            ),
            area,
        );
    }

    fn render_sidebar(&mut self, frame: &mut Frame, area: Rect) {
        let heading = Style::default()
            .fg(self.theme.text)
            .add_modifier(Modifier::BOLD);
        // Rendered without wrapping, so one pushed line is exactly one rendered row and
        // `clickable_row` can address a row by its index in `lines`. Every line carrying
        // variable-length text is ellipsised to the width the right border leaves, because a
        // line that wrapped would slide every row beneath it out from under the rectangles
        // this records for the pointer.
        let inner_width = usize::from(area.width).saturating_sub(1);
        let mut lines = vec![Line::styled("WORKSPACES", heading)];
        for (index, workspace) in self.layout.workspaces.iter().enumerate() {
            lines.push(Line::styled(
                format!(
                    "{} {}",
                    if index == self.workspace_index {
                        "›"
                    } else {
                        " "
                    },
                    ellipsise(&workspace.name, inner_width.saturating_sub(2))
                ),
                Style::default().fg(if index == self.workspace_index {
                    self.theme.accent
                } else {
                    self.theme.muted
                }),
            ));
        }
        lines.push(Line::from(""));
        lines.push(Line::styled("AGENTS", heading));
        // Sized so the leading glyph and its two spaces still fit inside the border.
        let label_width = inner_width.saturating_sub(3);
        let roster = self.agent_roster();
        let roster_is_empty = roster.is_empty();
        for (state, label) in roster {
            lines.push(Line::styled(
                format!(" {} {}", state.glyph(), ellipsise(label, label_width)),
                Style::default().fg(self.theme.agent(state)),
            ));
        }
        if roster_is_empty {
            lines.push(Line::styled(
                " none running",
                Style::default().fg(self.theme.muted),
            ));
        }
        lines.push(Line::from(""));
        lines.push(Line::styled("EXISTING AGENTS", heading));
        if self.external.is_empty() {
            lines.push(Line::styled(
                " none discovered",
                Style::default().fg(self.theme.muted),
            ));
        }
        for candidate in &self.external {
            lines.push(Line::styled(
                ellipsise(candidate.provider.as_str(), inner_width),
                Style::default().fg(self.theme.text),
            ));
            lines.push(Line::styled(
                ellipsise(candidate.status(), inner_width),
                Style::default().fg(self.theme.working),
            ));
        }
        if !self.external.is_empty() {
            lines.push(Line::styled(
                " click to dismiss all",
                Style::default().fg(self.theme.accent),
            ));
            self.dismiss_external_area = clickable_row(area, lines.len() - 1);
        }
        lines.push(Line::from(""));
        lines.push(Line::styled(
            "Ctrl+B l LAUNCH AGENT",
            Style::default()
                .fg(self.theme.accent)
                .add_modifier(Modifier::BOLD),
        ));
        self.launch_area = clickable_row(area, lines.len() - 1);
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::RIGHT)
                    .border_style(Style::default().fg(self.theme.border)),
            ),
            area,
        );
    }

    /// Live agents ordered by how much they are costing the user: blocked first, then by
    /// label so the list does not reshuffle between frames for equally urgent agents.
    ///
    /// Only a run whose agent was actually detected is an agent. `self.agents` also carries
    /// every pane's ambient shell, which reports no kind at all; listing those turned the
    /// roster into a list of run ids for processes that are not agents.
    fn agent_roster(&self) -> Vec<(AgentState, &str)> {
        let mut roster: Vec<(AgentState, &str)> = self
            .agents
            .values()
            .filter_map(|(kind, state)| Some((*state, kind.as_ref()?.label())))
            .collect();
        roster.sort_by(|left, right| {
            left.0
                .attention_rank()
                .cmp(&right.0.attention_rank())
                .then_with(|| left.1.cmp(right.1))
        });
        roster
    }

    fn render_node(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        workspace: &WorkspaceLayout,
        node: &LayoutNode,
    ) {
        match node {
            LayoutNode::Pane { pane_id } => {
                self.pane_areas.insert(pane_id.clone(), area);
                let pane = &workspace.panes[pane_id];
                let focused = workspace.focused_pane_id == *pane_id;
                let run_id = pane.run_id.clone();
                let (agent, state) = run_id
                    .as_deref()
                    .and_then(|id| self.agents.get(id).copied())
                    .unwrap_or((None, AgentState::Idle));
                let label = agent.map_or_else(|| pane.name.clone(), |kind| kind.label().to_owned());
                // A pane whose process is gone keeps painting its last frame forever. Without
                // this the only difference between a live shell and a dead one is that typing
                // stops working, so the title has to carry the news and the recovery key.
                let exited = pane.runtime == PaneRuntime::Exited;
                let title = if exited {
                    format!(" ✗ {label} · exited · Ctrl+B R restarts ")
                } else {
                    format!(
                        " {} {} · {} ",
                        state.glyph(),
                        label,
                        self.pane_location(pane)
                    )
                };
                let title_colour = if exited {
                    self.theme.blocked
                } else {
                    self.theme.agent(state)
                };
                // Copy mode swallows every key for this pane, so the pane itself has to say
                // so. Cloned rather than borrowed because the render below needs `self`
                // mutably for the resize bookkeeping.
                let copy_session = run_id
                    .as_deref()
                    .and_then(|id| self.copy.as_ref().filter(|session| session.run_id == id))
                    .cloned();
                // `title` already opens with a space, so the prefix needs none of its own.
                let title = match &copy_session {
                    Some(_) => Line::from(vec![
                        Span::styled(
                            " COPY",
                            Style::default()
                                .fg(self.theme.accent)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(title),
                    ]),
                    None => Line::from(title),
                };
                let block = Block::default()
                    .borders(Borders::ALL)
                    .border_type(Theme::border_type())
                    .title(title)
                    .title_style(Style::default().fg(title_colour))
                    .border_style(Style::default().fg(if focused {
                        self.theme.border_focused
                    } else {
                        self.theme.border
                    }));
                let inner = block.inner(area);
                self.queue_resize(&workspace.workspace_id, pane_id, run_id.as_deref(), inner);
                frame.render_widget(block, area);
                self.pane_inner_areas.insert(pane_id.clone(), inner);
                match run_id.as_deref().and_then(|id| self.screens.get(id)) {
                    Some(screen) => {
                        // The cursor belongs to whichever pane is taking keystrokes; drawing
                        // one in every pane would make focus unreadable. In copy mode the
                        // PTY's own cursor is hidden too: the copy cursor is the one that
                        // moves, and two blocks would make it ambiguous which is which.
                        let mut cursor = Cursor::default();
                        if !focused || copy_session.is_some() {
                            cursor.hide();
                        }
                        frame.render_widget(
                            PseudoTerminal::new(screen.screen()).cursor(cursor),
                            inner,
                        );
                        if let Some(session) = &copy_session {
                            self.render_copy_overlay(frame, inner, session);
                        }
                    }
                    None => {
                        let mut placeholder = vec![Line::styled(
                            "starting…",
                            Style::default().fg(self.theme.muted),
                        )];
                        if run_id.is_none() {
                            placeholder.push(Line::styled(
                                "Ctrl+B R starts a shell · Ctrl+B l launches an agent here",
                                Style::default().fg(self.theme.muted),
                            ));
                        }
                        frame.render_widget(
                            Paragraph::new(placeholder).wrap(Wrap { trim: true }),
                            inner,
                        );
                    }
                }
            }
            LayoutNode::Split {
                axis,
                ratio_milli,
                first,
                second,
            } => {
                let (a, divider, b) = split_rect(area, *axis, *ratio_milli);
                let resize_pane = first_leaf(second).to_owned();
                self.dividers.push(Divider {
                    area: divider,
                    pane_id: resize_pane,
                    axis: *axis,
                    container: area,
                });
                self.render_node(frame, a, workspace, first);
                self.render_node(frame, b, workspace, second);
            }
        }
    }

    /// Paints the selection run and the copy cursor over the emulated screen.
    ///
    /// Only backgrounds are touched: `PseudoTerminal` stays the single thing that draws the
    /// text, so a highlight can never disagree with what the pane actually shows.
    fn render_copy_overlay(&self, frame: &mut Frame, inner: Rect, session: &CopySession) {
        let buffer = frame.buffer_mut();
        if let Some((from, to)) = session.selection() {
            let (start, end) = if from <= to { (from, to) } else { (to, from) };
            for row in start.0..=end.0 {
                // Reading order, not a rectangle: whole rows in the middle and only the
                // anchored halves of the first and last. This is the same run
                // `VtTerminal::selection_text` extracts, so the highlight previews the yank
                // rather than offering a second opinion about it.
                let first = if row == start.0 { start.1 } else { 0 };
                let last = if row == end.0 {
                    end.1
                } else {
                    inner.width.saturating_sub(1)
                };
                for column in first..=last {
                    if let Some(cell) = cell_at(buffer, inner, row, column) {
                        cell.set_bg(self.theme.selection);
                    }
                }
            }
        }
        let (row, column) = session.cursor();
        if let Some(cell) = cell_at(buffer, inner, row, column) {
            cell.set_bg(self.theme.accent).set_fg(self.theme.surface);
        }
    }

    /// The binding facts the pane body used to spell out line by line. They survive as a
    /// title suffix because the body is now the emulated screen and has no room for them.
    fn pane_location(&self, pane: &PaneLayout) -> String {
        let Some(run_id) = pane.run_id.as_deref() else {
            return "unbound".into();
        };
        let Some(run) = self.runs.iter().find(|run| run.run_id == run_id) else {
            return format!("{run_id} · unavailable");
        };
        match run.binding_kind {
            BindingKind::Terminal => run_id.to_owned(),
            BindingKind::Repository => format!("{run_id} · {}", run.external_task_ref),
        }
    }

    /// Announces a pane's inner geometry to the daemon, but only when it actually changed.
    /// `render` runs on every frame, so re-sending an identical size would put one resize
    /// request per pane per frame onto a socket that is otherwise silent when nothing moves.
    ///
    /// The record is keyed by run as well as size: a pane that just gained a run must be
    /// announced even at unchanged geometry, because the new PTY started at the daemon's
    /// own default rather than at this pane's size.
    fn queue_resize(
        &mut self,
        workspace_id: &str,
        pane_id: &str,
        run_id: Option<&str>,
        inner: Rect,
    ) {
        let Some(run_id) = run_id else {
            return;
        };
        let geometry = (run_id.to_owned(), inner.height, inner.width);
        if self.pane_geometry.get(pane_id) == Some(&geometry) {
            return;
        }
        self.pane_geometry.insert(pane_id.to_owned(), geometry);
        self.pending_resizes.push((
            workspace_id.to_owned(),
            pane_id.to_owned(),
            inner.height,
            inner.width,
        ));
    }

    fn render_narrow(&self, frame: &mut Frame, area: Rect) {
        let mut lines = vec![Line::styled(
            "d·ock · compact runtime",
            Style::default().fg(self.theme.accent),
        )];
        if let Some(workspace) = self.workspace() {
            lines.push(Line::from(format!(
                "{} · {} panes",
                workspace.name,
                workspace.panes.len()
            )));
            if let Some(pane) = workspace.panes.get(&workspace.focused_pane_id) {
                lines.push(Line::styled(
                    format!(
                        "› {} · {} · focused",
                        pane.name,
                        runtime_label(pane.runtime)
                    ),
                    Style::default().fg(self.theme.accent),
                ));
            }
        } else {
            lines.push(Line::from("No workspace · n create"));
        }
        lines.push(Line::styled(
            "Ctrl+B then n new · h/v split · Tab focus · l launch · ? help · q quit",
            Style::default().fg(self.theme.muted),
        ));
        frame.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: true }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(Theme::border_type())
                    .border_style(Style::default().fg(self.theme.border)),
            ),
            area,
        );
    }

    fn render_help(&self, frame: &mut Frame, area: Rect) {
        let width = area.width.min(68);
        let height = area.height.min(18);
        let popup = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        let heading = Style::default()
            .fg(self.theme.accent)
            .add_modifier(Modifier::BOLD);
        let lines = vec![
            Line::styled("TYPING", heading),
            Line::from("Every key goes to the focused pane, Esc and Ctrl-C included."),
            Line::from("Ctrl+B is the only key Dock keeps; Ctrl+B Ctrl+B sends a literal one."),
            Line::styled("AFTER Ctrl+B", heading),
            Line::from("n new workspace   h/v split   z zoom"),
            Line::from("r rename   R restart shell   x close   l launch   q quit   ,/. workspace"),
            Line::from("[ copy mode: hjkl move   v select   y yank   / search   Esc exits"),
            Line::from("d leaves the dashboard; runs keep running until you close them."),
            Line::from("Tab/S-Tab or arrows focus   +/- resize"),
            Line::styled("FORMS", heading),
            Line::from("type to filter/edit   ↑/↓ or j/k select   Enter review/confirm"),
            Line::from("Esc cancels a form rather than reaching the pane while one is open."),
            Line::styled("CURRENT", heading),
            Line::from(if self.workspace().is_some() {
                "Workspace selected; pane commands are available."
            } else {
                "No workspace: create one with Ctrl+B n before pane actions."
            }),
            Line::from("Esc or ? closes help"),
        ];
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: true })
                .style(Style::default().fg(self.theme.text))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(Theme::border_type())
                        .border_style(Style::default().fg(self.theme.border_focused))
                        .title(" KEYMAP "),
                ),
            popup,
        );
    }

    fn render_rename(&self, frame: &mut Frame, area: Rect) {
        let width = area.width.min(48);
        let popup = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + area.height.saturating_sub(5) / 2,
            width,
            5,
        );
        let value = self.rename_form.as_deref().unwrap_or_default();
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(format!("Name: {value}█")),
                Line::from("Enter saves · Esc cancels"),
            ])
            .style(Style::default().fg(self.theme.text))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(Theme::border_type())
                    .border_style(Style::default().fg(self.theme.border_focused))
                    .title(" RENAME FOCUSED PANE "),
            ),
            popup,
        );
    }

    pub fn key(&mut self, key: KeyEvent) -> UiCommand {
        if self.help_open {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('?')) {
                self.help_open = false;
            }
            return UiCommand::None;
        }
        if self.rename_form.is_some() {
            return self.rename_key(key);
        }
        if self.launch_form.is_some() {
            return self.launch_key(key);
        }
        // Ahead of the keymap on purpose: copy mode owns every key while it is active, so its
        // motions (`h`, `j`, `k`, `l`) and its verbs (`v`, `y`) can never be forwarded to the
        // PTY as ordinary input.
        if self.copy.is_some() {
            return self.copy_key(key);
        }
        let encoding = self.encoding_for_focused_pane();
        match self.keymap.handle(key, encoding) {
            // Deliberately not a `Request`: pane input is fire-and-forget, and routing it
            // through the request arm would put two daemon round trips in front of the echo.
            // Dropped outright when the pane has no run: there is no PTY to receive it, and
            // sending anyway earns one daemon error per character straight into the footer.
            KeyOutcome::Passthrough(bytes) => self.send_to_pane(bytes),
            KeyOutcome::Command(command) => self.run_command(command),
            KeyOutcome::PendingPrefix | KeyOutcome::Ignored => UiCommand::None,
        }
    }

    fn run_command(&mut self, command: PaneCommand) -> UiCommand {
        match command {
            PaneCommand::NewWorkspace => {
                let workspace_id = self.next_unique_id("workspace");
                let pane_id = self.next_unique_id("pane");
                UiCommand::Request(Box::new(Request::Workspace(WorkspaceRequest::Create {
                    name: workspace_id.replace('_', " "),
                    workspace_id,
                    pane_id,
                })))
            }
            PaneCommand::Split(axis) => self.split(axis),
            // Focus is still ordinal rather than geometric, so the two backwards directions
            // and the two forwards ones collapse onto the existing cycle.
            PaneCommand::Focus(direction) => self.focus_next(matches!(
                direction,
                FocusDirection::Previous | FocusDirection::Left | FocusDirection::Up
            )),
            PaneCommand::Workspace(delta) => self.select_workspace(delta),
            PaneCommand::Resize(delta) => self.resize_keyboard(delta),
            PaneCommand::Zoom => self.zoom(),
            PaneCommand::Rename => self.rename(),
            PaneCommand::Close => self.close(),
            PaneCommand::Respawn => self.respawn(),
            PaneCommand::Launch => {
                self.open_launch();
                UiCommand::LoadCatalog
            }
            PaneCommand::CopyMode => self.enter_copy_mode(),
            // The daemon owns every run, so leaving the dashboard signals nothing and tears
            // nothing down. Detaching and quitting are therefore the same act here.
            PaneCommand::Detach | PaneCommand::Quit => UiCommand::Quit,
            PaneCommand::Help => {
                self.error = None;
                self.help_open = true;
                UiCommand::None
            }
        }
    }

    /// Moves the visible workspace, saturating at both ends rather than wrapping: `.` on the
    /// last workspace is a mis-press, and jumping silently back to the first would move the
    /// user somewhere they did not ask to go.
    ///
    /// Purely local, like zoom — the daemon has no notion of which workspace this client is
    /// looking at, so there is nothing to tell it.
    fn select_workspace(&mut self, delta: i8) -> UiCommand {
        let Some(last) = self.layout.workspaces.len().checked_sub(1) else {
            self.error = Some("workspace unavailable: create a workspace first".into());
            return UiCommand::None;
        };
        self.workspace_index = if delta < 0 {
            self.workspace_index
                .saturating_sub(usize::from(delta.unsigned_abs()))
        } else {
            self.workspace_index
                .saturating_add(usize::from(delta.unsigned_abs()))
                .min(last)
        };
        self.error = None;
        UiCommand::None
    }

    /// Toggles a full-area view of the focused pane. Zoom is local to this client: the
    /// daemon's layout tree is unchanged, so it costs no request, but the pane's inner
    /// geometry does change and the next frame announces the new PTY size.
    fn zoom(&mut self) -> UiCommand {
        let Some(workspace) = self.workspace() else {
            self.error = Some("zoom unavailable: create a workspace first".into());
            return UiCommand::None;
        };
        let focused = workspace.focused_pane_id.clone();
        self.zoomed = match self.zoomed.take() {
            Some(current) if current == focused => None,
            _ => Some(focused),
        };
        self.error = None;
        UiCommand::None
    }

    /// Whether the focused pane's viewport is frozen for selection.
    pub fn copy_mode(&self) -> bool {
        self.copy.is_some()
    }

    /// The mode indicator. Copy mode swallows every key, so it has to announce itself: an
    /// input mode with no on-screen trace is indistinguishable from a hung dashboard.
    pub fn copy_status(&self) -> Option<String> {
        let session = self.copy.as_ref()?;
        if self.copy_searching {
            return Some(format!(
                "COPY /{}",
                session.search_query().unwrap_or_default()
            ));
        }
        let (row, column) = session.cursor();
        let verb = if session.selecting() {
            "SELECTING"
        } else {
            "MOVE"
        };
        // CONTROLLER ADDENDUM: grid coordinates stay 0-based everywhere inside; only this
        // presentation boundary counts from one, because that is how every editor and pager
        // the user knows reports a line and column.
        Some(format!(
            "COPY {verb} {},{}",
            row.saturating_add(1),
            column.saturating_add(1)
        ))
    }

    /// Freezes the focused pane for keyboard selection, starting at its live cursor.
    fn enter_copy_mode(&mut self) -> UiCommand {
        let Some(run_id) = self.focused_run_id().map(str::to_owned) else {
            self.error =
                Some("copy mode unavailable: this pane has no run · Ctrl+B l launches one".into());
            return UiCommand::None;
        };
        let cursor = self
            .screens
            .get(&run_id)
            .map(PaneScreen::cursor)
            .unwrap_or((0, 0));
        self.copy = Some(CopySession::new(run_id, cursor));
        self.copy_searching = false;
        self.error = None;
        UiCommand::None
    }

    /// Every key while copy mode is active, so none of them can reach the PTY.
    ///
    /// The session is taken out of `self` for the duration: the handlers need the pane's
    /// screen at the same time, and leaving the session in place would borrow `self` twice.
    fn copy_key(&mut self, key: KeyEvent) -> UiCommand {
        let Some(mut session) = self.copy.take() else {
            return UiCommand::None;
        };
        let bounds = self
            .screens
            .get(&session.run_id)
            .map(PaneScreen::size)
            .unwrap_or((0, 0));
        // Esc unwinds one level at a time: the prompt first, then the mode. The invariant is
        // that a small bounded number of presses always reaches the live pane, not that one
        // press escapes every level — which is exactly what a rename form already does.
        if key.code == KeyCode::Esc && !self.copy_searching {
            self.leave_copy_mode(&session);
            return UiCommand::None;
        }
        if self.copy_searching {
            self.copy_search_key(key, &mut session, bounds);
            self.copy = Some(session);
            return UiCommand::None;
        }
        match key.code {
            // A composed letter is somebody reaching past copy mode, not a motion: without
            // this `Ctrl+H` moves left and `Ctrl+Y` yanks. Shift stays allowed because
            // crossterm reports uppercase `G` and `N` with it set.
            KeyCode::Char(_) if composed(key) => {}
            KeyCode::Char('q') => {
                self.leave_copy_mode(&session);
                return UiCommand::None;
            }
            KeyCode::Char('h') | KeyCode::Left => self.copy_move(&mut session, 0, -1, bounds),
            KeyCode::Char('j') | KeyCode::Down => self.copy_move(&mut session, 1, 0, bounds),
            KeyCode::Char('k') | KeyCode::Up => self.copy_move(&mut session, -1, 0, bounds),
            KeyCode::Char('l') | KeyCode::Right => self.copy_move(&mut session, 0, 1, bounds),
            // Top and bottom of what the replica actually holds: the client's own grid has no
            // scrollback of its own yet, so these are the visible extremes rather than history.
            KeyCode::Char('g') => session.set_cursor((0, 0), bounds),
            KeyCode::Char('G') => session.set_cursor((bounds.0.saturating_sub(1), 0), bounds),
            KeyCode::Char('v') => session.begin_selection(),
            KeyCode::Char('y') => {
                self.yank(&session);
                self.leave_copy_mode(&session);
                return UiCommand::None;
            }
            KeyCode::Char('/') => {
                session.begin_search();
                self.copy_searching = true;
                self.error = None;
            }
            KeyCode::Char('n') => self.copy_jump(&mut session, true, bounds),
            KeyCode::Char('N') => self.copy_jump(&mut session, false, bounds),
            _ => {}
        }
        self.copy = Some(session);
        UiCommand::None
    }

    /// Keys typed at the `/` prompt. Enter closes the prompt and jumps; the query survives so
    /// `n`/`N` can keep walking the same matches.
    fn copy_search_key(&mut self, key: KeyEvent, session: &mut CopySession, bounds: (u16, u16)) {
        match key.code {
            // Same rule as the mode's own bindings: `Ctrl+C` must not type a `c` into the query.
            KeyCode::Char(_) if composed(key) => {}
            KeyCode::Char(character) => session.push_search(character),
            KeyCode::Esc => {
                session.cancel_search();
                self.copy_searching = false;
            }
            KeyCode::Backspace => {
                if session.search_query().is_some_and(str::is_empty) {
                    session.cancel_search();
                    self.copy_searching = false;
                } else {
                    session.pop_search();
                }
            }
            KeyCode::Enter => {
                self.copy_searching = false;
                self.copy_jump(session, true, bounds);
            }
            _ => {}
        }
    }

    /// Moves the copy cursor, pulling the viewport through scrollback when it walks off an
    /// edge so the cursor never leaves the rows on screen.
    fn copy_move(&mut self, session: &mut CopySession, rows: i32, cols: i32, bounds: (u16, u16)) {
        let (row, _) = session.cursor();
        let edge = if rows < 0 && row == 0 {
            1
        } else if rows > 0 && row + 1 >= bounds.0 {
            -1
        } else {
            0
        };
        if edge != 0
            && let Some(screen) = self.screens.get_mut(&session.run_id)
        {
            screen.scroll_by(edge);
        }
        session.move_cursor(rows, cols, bounds);
    }

    /// Jumps to the next or previous hit for the standing query, or says why it could not.
    fn copy_jump(&mut self, session: &mut CopySession, forward: bool, bounds: (u16, u16)) {
        let Some(query) = session.search_query().map(str::to_owned) else {
            self.error = Some("no search yet · / starts one".into());
            return;
        };
        let rows: Vec<String> = self
            .screens
            .get(&session.run_id)
            .map(|screen| (0..bounds.0).map(|row| screen.visible_row(row)).collect())
            .unwrap_or_default();
        if session.jump_to_match(&find_matches(&rows, &query), forward, bounds) {
            self.error = None;
        } else {
            self.error = Some(format!("no matches for {query:?}"));
        }
    }

    /// Puts the selection on the clipboard and names the route it took.
    ///
    /// A bare `y` with no anchor yanks the cursor's line rather than refusing: the dominant
    /// reason to press `y` in a terminal is "give me that line" — a URL, a path, a stack
    /// frame — and demanding `v$y` for it is friction on the most common copy there is. The
    /// line yank names itself in the notice so a user who meant something else sees it at once.
    ///
    /// The route is reported because OSC 52 is disabled by default in some terminals: a yank
    /// that silently reached nothing looks exactly like a yank that worked.
    fn yank(&mut self, session: &CopySession) {
        let screen = self.screens.get(&session.run_id);
        let (text, subject) = match session.selection() {
            Some((from, to)) => {
                let text = screen
                    .map(|screen| screen.selection_text(from, to))
                    .unwrap_or_default();
                let count = text.chars().count();
                (text, format!("{count} characters"))
            }
            None => {
                let row = session.cursor().0;
                // Trailing blanks are grid padding, not content: nobody wants 60 spaces
                // pasted after the path they just copied.
                let text = screen
                    .map(|screen| screen.visible_row(row).trim_end().to_owned())
                    .unwrap_or_default();
                let count = text.chars().count();
                // 1-based for the same reason `copy_status` is, and it has to agree with it:
                // seeing the same line called 0 in one place and 1 in another reads as a bug.
                (
                    text,
                    format!("line {} ({count} characters)", row.saturating_add(1)),
                )
            }
        };
        self.error = Some(match clipboard::copy(&text) {
            Ok(route) => {
                let route = match route {
                    ClipboardRoute::Osc52 => "OSC 52",
                    ClipboardRoute::Command(helper) => helper,
                };
                format!("copied {subject} to the clipboard via {route}")
            }
            Err(reason) => format!("copy failed: {reason}"),
        });
    }

    /// Extends a pointer selection, entering copy mode on the first drag of the gesture.
    ///
    /// The anchor is re-applied on every event rather than only on the first: it is always
    /// the cell the button went down on, whatever the cursor was doing beforehand, and
    /// re-applying it is cheaper than tracking whether this drag has already anchored.
    fn drag_selection(&mut self, drag: &PaneDrag, column: u16, row: u16) {
        let Some(bounds) = self.screens.get(&drag.run_id).map(PaneScreen::size) else {
            return;
        };
        if let Some(existing) = self.copy.clone()
            && existing.run_id != drag.run_id
        {
            // Dragging in a different pane hands copy mode over; the pane being left goes
            // back to following live output rather than staying silently frozen.
            self.leave_copy_mode(&existing);
        }
        if self.copy.is_none() {
            self.copy = Some(CopySession::new(drag.run_id.clone(), drag.origin));
            self.copy_searching = false;
            self.error = None;
        }
        let Some(session) = self.copy.as_mut() else {
            return;
        };
        session.set_cursor(drag.origin, bounds);
        session.begin_selection();
        session.set_cursor(clamp_cell(drag.inner, column, row), bounds);
    }

    /// Leaves copy mode and returns the pane to the live tail, which is where the user was
    /// before they froze it.
    fn leave_copy_mode(&mut self, session: &CopySession) {
        if let Some(screen) = self.screens.get_mut(&session.run_id) {
            screen.scroll_to_live();
        }
        self.copy = None;
        self.copy_searching = false;
    }

    /// The focused pane's own key encoding. A program that sets DECCKM changes what bytes
    /// an arrow key must produce, so this is read per pane rather than assumed globally.
    fn encoding_for_focused_pane(&self) -> KeyEncoding {
        self.focused_run_id()
            .and_then(|run_id| self.screens.get(run_id))
            .map(|screen| KeyEncoding {
                application_cursor: screen.screen().application_cursor(),
            })
            .unwrap_or_default()
    }

    fn focused_run_id(&self) -> Option<&str> {
        self.focused_pane()?.run_id.as_deref()
    }

    fn focused_pane(&self) -> Option<&PaneLayout> {
        let workspace = self.workspace()?;
        workspace.panes.get(&workspace.focused_pane_id)
    }

    /// Routes bytes to the focused pane, or explains why they went nowhere.
    ///
    /// Input aimed at a pane with no live process used to be discarded in silence, which is
    /// indistinguishable from a frozen dashboard: the pane keeps painting the dead shell's last
    /// frame and typing simply stops having any effect. Every rejection now names the key that
    /// gets the pane working again.
    fn send_to_pane(&mut self, bytes: Vec<u8>) -> UiCommand {
        match self.focused_pane() {
            Some(pane) if pane.runtime == PaneRuntime::Exited => {
                self.error = Some("pane exited · Ctrl+B R restarts a shell here".into());
                UiCommand::None
            }
            // A pane that never had a run is not a pane that stopped working, and the daemon
            // would answer every character with an error that flickers through the footer. The
            // pane body already carries the two keys that give it a process.
            Some(pane) if pane.run_id.is_none() => UiCommand::None,
            Some(_) => UiCommand::PaneInput(bytes),
            None => UiCommand::None,
        }
    }

    /// A pasted payload, wrapped for the focused pane's own bracketed-paste mode.
    ///
    /// Without this the host terminal delivers a paste as individual key events and each line
    /// executes as it lands, which is precisely the paste-injection hazard `encode_paste` was
    /// written to close — reached by a route that never called it.
    pub fn paste(&mut self, text: String) -> UiCommand {
        if text.is_empty() {
            return UiCommand::None;
        }
        // Whether to bracket is the receiving application's decision, not this client's: an
        // application that never enabled the mode would read the wrapper as literal input.
        let bracketed = self
            .focused_run_id()
            .and_then(|run_id| self.screens.get(run_id))
            .is_some_and(PaneScreen::bracketed_paste);
        self.send_to_pane(encode_paste(&text, bracketed))
    }

    /// Asks the daemon for a fresh Dock-owned shell in the focused pane.
    fn respawn(&mut self) -> UiCommand {
        let Some(workspace) = self.workspace() else {
            self.error = Some("restart unavailable: create a workspace first".into());
            return UiCommand::None;
        };
        let workspace_id = workspace.workspace_id.clone();
        let pane_id = workspace.focused_pane_id.clone();
        self.error = None;
        UiCommand::Request(Box::new(Request::Workspace(WorkspaceRequest::Respawn {
            workspace_id,
            pane_id,
        })))
    }

    fn open_launch(&mut self) {
        self.error = None;
        self.launch_form = Some(LaunchForm {
            index: self.last_launch_profile.min(PROFILES.len() - 1),
            repository_mode: self.last_repository_mode && !self.repository_launches.is_empty(),
            confirming: false,
            query: String::new(),
        });
    }

    fn launch_key(&mut self, key: KeyEvent) -> UiCommand {
        let form = self.launch_form.as_mut().expect("launch form");
        match key.code {
            KeyCode::Esc => {
                self.launch_form = None;
                UiCommand::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                form.index = previous_matching(form.index, &form.query);
                form.confirming = false;
                UiCommand::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                form.index = next_matching(form.index, &form.query);
                form.confirming = false;
                UiCommand::None
            }
            KeyCode::Tab => {
                if self.repository_launches.is_empty() {
                    self.error = Some(
                        "repository mode unavailable: no verified repository/task/worktree option"
                            .into(),
                    );
                } else {
                    form.repository_mode = !form.repository_mode;
                    form.confirming = false;
                    self.error = None;
                }
                UiCommand::None
            }
            KeyCode::Char(character) if !form.confirming && !character.is_control() => {
                form.query.push(character);
                if let Some(index) = matching_profiles(&form.query).next() {
                    form.index = index;
                    self.error = None;
                } else {
                    self.error = Some(format!("no fixed provider matches ‘{}’", form.query));
                }
                UiCommand::None
            }
            KeyCode::Backspace if !form.confirming => {
                form.query.pop();
                if let Some(index) = matching_profiles(&form.query).next() {
                    form.index = index;
                    self.error = None;
                }
                UiCommand::None
            }
            KeyCode::Enter if !form.confirming => {
                if matching_profiles(&form.query).any(|index| index == form.index) {
                    form.confirming = true;
                    self.error = None;
                } else {
                    self.error = Some("launch unavailable: no provider matches the filter".into());
                }
                UiCommand::None
            }
            KeyCode::Enter => self.confirm_launch(),
            _ => UiCommand::None,
        }
    }

    fn confirm_launch(&mut self) -> UiCommand {
        let form = self.launch_form.clone().expect("launch form");
        let Some(workspace) = self.workspace() else {
            self.error = Some("cannot launch: create a workspace first".into());
            self.launch_form = None;
            return UiCommand::None;
        };
        let workspace_id = workspace.workspace_id.clone();
        let pane_id = workspace.focused_pane_id.clone();
        let run_id = self.next_unique_id("dock_ui");
        let profile = PROFILES[form.index].0;
        let id = AdapterId::from(profile);
        if !crate::adapter::builtin_available(&id) {
            self.error = Some(format!(
                "{} is unavailable: fixed executable not found",
                PROFILES[form.index].1
            ));
            return UiCommand::None;
        }
        self.last_launch_profile = form.index;
        self.last_repository_mode = form.repository_mode;
        self.launch_form = None;
        if !form.repository_mode {
            return UiCommand::Request(Box::new(Request::TerminalLaunch(TerminalLaunchRequest {
                workspace_id,
                pane_id,
                run_id,
                profile,
                runtime_directory: self.runtime_directory.clone(),
            })));
        }
        let Some(option) = self.repository_launches.first() else {
            self.error = Some("repository dispatch is unavailable".into());
            return UiCommand::None;
        };
        UiCommand::Request(Box::new(Request::LaunchIntoPane(LaunchIntoPaneRequest {
            workspace_id,
            pane_id,
            dispatch: DispatchRequest {
                repository_root: self.repository_root.clone(),
                external_task_ref: option.task_ref.clone(),
                run_id,
                worktree: option.worktree.clone(),
                adapter: AdapterSelection {
                    id,
                    executable: None,
                    arguments: if profile == DashboardProfile::Fixture {
                        vec![
                            "-c".into(),
                            "printf 'Dock-owned fixture ready\\n'; sleep 30".into(),
                        ]
                    } else {
                        vec![]
                    },
                },
            },
        })))
    }

    fn render_launch_form(&mut self, frame: &mut Frame, area: Rect) {
        let form = self.launch_form.as_ref().expect("launch form");
        let width = area.width.min(58);
        let height = area.height.min(13);
        let popup = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        let target = self
            .workspace()
            .map(|workspace| format!("{}/{}", workspace.name, workspace.focused_pane_id))
            .unwrap_or_else(|| "unavailable (create workspace first)".into());
        let mut lines = vec![Line::from(format!(
            "Mode: {}  [Tab] toggle · Target: {}",
            if form.repository_mode {
                "repository-bound"
            } else {
                "unbound terminal"
            },
            target
        ))];
        self.launch_mode_area = Some(Rect::new(
            popup.x + 1,
            popup.y + 1,
            popup.width.saturating_sub(2),
            1,
        ));
        self.launch_profile_areas = (0..PROFILES.len())
            .map(|index| {
                Rect::new(
                    popup.x + 1,
                    popup.y + 2 + index as u16,
                    popup.width.saturating_sub(2),
                    1,
                )
            })
            .collect();
        for (index, (profile, label)) in PROFILES.iter().enumerate() {
            let available = crate::adapter::builtin_available(&AdapterId::from(*profile));
            let matches = profile_matches(index, &form.query);
            lines.push(Line::styled(
                format!(
                    "{} {} — {}",
                    if index == form.index && matches {
                        "›"
                    } else {
                        " "
                    },
                    label,
                    if available {
                        "available"
                    } else {
                        "unavailable: fixed executable not found"
                    }
                ),
                Style::default().fg(if !matches {
                    self.theme.surface
                } else if available {
                    self.theme.accent
                } else {
                    self.theme.muted
                }),
            ));
        }
        lines.push(Line::from(if form.confirming {
            format!(
                "REVIEW {} → {} · Enter launches · Esc cancels",
                PROFILES[form.index].1, target
            )
        } else {
            format!(
                "Filter: {}█ · type, ↑/↓/j/k select · Enter review · Esc cancels",
                form.query
            )
        }));
        self.launch_confirm_area = Some(Rect::new(
            popup.x + 1,
            popup.y + 2 + PROFILES.len() as u16,
            popup.width.saturating_sub(2),
            1,
        ));
        frame.render_widget(
            Paragraph::new(lines)
                .style(Style::default().fg(self.theme.text))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(Theme::border_type())
                        .border_style(Style::default().fg(self.theme.border_focused))
                        .title(" LAUNCH FIXED PROFILE "),
                ),
            popup,
        );
    }

    fn focus_next(&mut self, reverse: bool) -> UiCommand {
        let Some(workspace) = self.workspace() else {
            self.error = Some("focus unavailable: create a workspace first".into());
            return UiCommand::None;
        };
        let ids: Vec<_> = workspace.panes.keys().collect();
        let current = ids
            .iter()
            .position(|id| ***id == workspace.focused_pane_id)
            .unwrap_or(0);
        let next = if reverse {
            current
                .checked_sub(1)
                .unwrap_or(ids.len().saturating_sub(1))
        } else {
            (current + 1) % ids.len()
        };
        let workspace_id = workspace.workspace_id.clone();
        let pane_id = ids[next].to_string();
        self.layout.workspaces[self.workspace_index].focused_pane_id = pane_id.clone();
        self.error = None;
        UiCommand::Request(Box::new(Request::Workspace(WorkspaceRequest::Focus {
            workspace_id,
            pane_id,
        })))
    }

    fn split(&mut self, axis: SplitAxis) -> UiCommand {
        let Some((workspace_id, pane_id)) = self.workspace().map(|workspace| {
            (
                workspace.workspace_id.clone(),
                workspace.focused_pane_id.clone(),
            )
        }) else {
            self.error = Some("split unavailable: create a workspace first".into());
            return UiCommand::None;
        };
        let new_pane_id = self.next_unique_id("pane");
        let workspace = &mut self.layout.workspaces[self.workspace_index];
        split_leaf(&mut workspace.root, &pane_id, new_pane_id.clone(), axis);
        workspace.panes.insert(
            new_pane_id.clone(),
            PaneLayout {
                pane_id: new_pane_id.clone(),
                name: new_pane_id.replace('_', " "),
                run_id: None,
                runtime: PaneRuntime::Empty,
            },
        );
        workspace.focused_pane_id = new_pane_id.clone();
        UiCommand::Request(Box::new(Request::Workspace(WorkspaceRequest::Split {
            workspace_id,
            pane_id,
            new_pane_id,
            axis,
        })))
    }

    fn rename(&mut self) -> UiCommand {
        let Some(workspace) = self.workspace() else {
            self.error = Some("rename unavailable: create a workspace first".into());
            return UiCommand::None;
        };
        self.rename_form = Some(workspace.panes[&workspace.focused_pane_id].name.clone());
        self.error = None;
        UiCommand::None
    }

    fn close(&mut self) -> UiCommand {
        let Some(workspace) = self.workspace() else {
            self.error = Some("close unavailable: create a workspace first".into());
            return UiCommand::None;
        };
        UiCommand::Request(Box::new(Request::Workspace(WorkspaceRequest::Close {
            workspace_id: workspace.workspace_id.clone(),
            pane_id: workspace.focused_pane_id.clone(),
        })))
    }

    fn rename_key(&mut self, key: KeyEvent) -> UiCommand {
        match key.code {
            KeyCode::Esc => {
                self.rename_form = None;
                self.error = None;
                UiCommand::None
            }
            KeyCode::Backspace => {
                self.rename_form.as_mut().expect("rename form").pop();
                UiCommand::None
            }
            KeyCode::Char(character) if !character.is_control() => {
                let value = self.rename_form.as_mut().expect("rename form");
                if value.chars().count() < 80 {
                    value.push(character);
                }
                UiCommand::None
            }
            KeyCode::Enter => {
                let name = self
                    .rename_form
                    .as_ref()
                    .expect("rename form")
                    .trim()
                    .to_owned();
                if name.is_empty() {
                    self.error = Some("rename unavailable: name cannot be empty".into());
                    return UiCommand::None;
                }
                let workspace = self
                    .workspace()
                    .expect("workspace retained while form open");
                let workspace_id = workspace.workspace_id.clone();
                let pane_id = workspace.focused_pane_id.clone();
                self.layout.workspaces[self.workspace_index]
                    .panes
                    .get_mut(&pane_id)
                    .expect("focused pane")
                    .name = name.clone();
                self.rename_form = None;
                self.error = None;
                UiCommand::Request(Box::new(Request::Workspace(WorkspaceRequest::Rename {
                    workspace_id,
                    pane_id: Some(pane_id),
                    name,
                })))
            }
            _ => UiCommand::None,
        }
    }

    fn resize_keyboard(&mut self, delta: i16) -> UiCommand {
        let Some(workspace) = self.workspace() else {
            self.error = Some("resize unavailable: create a split workspace first".into());
            return UiCommand::None;
        };
        let workspace_id = workspace.workspace_id.clone();
        let pane_id = workspace.focused_pane_id.clone();
        let Some(ratio) = adjust_parent_ratio(
            &mut self.layout.workspaces[self.workspace_index].root,
            &pane_id,
            delta,
        ) else {
            self.error = Some("resize unavailable: focused pane has no split divider".into());
            return UiCommand::None;
        };
        self.error = None;
        UiCommand::Request(Box::new(Request::Workspace(WorkspaceRequest::Resize {
            workspace_id,
            pane_id,
            ratio_milli: ratio,
        })))
    }

    pub fn mouse(&mut self, event: MouseEvent) -> UiCommand {
        if self.launch_form.is_some() {
            if event.kind == MouseEventKind::Down(MouseButton::Left) {
                if self
                    .launch_mode_area
                    .is_some_and(|area| contains(area, event.column, event.row))
                {
                    if self.repository_launches.is_empty() {
                        self.error = Some("repository mode unavailable: no verified repository/task/worktree option".into());
                    } else {
                        let form = self.launch_form.as_mut().expect("launch form");
                        form.repository_mode = !form.repository_mode;
                        form.confirming = false;
                        self.error = None;
                    }
                    return UiCommand::None;
                }
                if let Some(index) = self
                    .launch_profile_areas
                    .iter()
                    .position(|area| contains(*area, event.column, event.row))
                {
                    let form = self.launch_form.as_mut().expect("launch form");
                    form.index = index;
                    form.confirming = false;
                    return UiCommand::None;
                }
                if self
                    .launch_confirm_area
                    .is_some_and(|area| contains(area, event.column, event.row))
                {
                    if self
                        .launch_form
                        .as_ref()
                        .is_some_and(|form| form.confirming)
                    {
                        return self.confirm_launch();
                    }
                    self.launch_form.as_mut().expect("launch form").confirming = true;
                }
            }
            return UiCommand::None;
        }
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                // A fresh press ends any previous gesture, whatever swallowed its release.
                // Without this a stale arming would hijack the next divider drag.
                self.pane_drag = None;
                if self
                    .dismiss_external_area
                    .is_some_and(|area| contains(area, event.column, event.row))
                {
                    self.external.clear();
                    return UiCommand::None;
                }
                if self
                    .launch_area
                    .is_some_and(|area| contains(area, event.column, event.row))
                {
                    self.open_launch();
                    return UiCommand::LoadCatalog;
                }
                if let Some(divider) = self
                    .dividers
                    .iter()
                    .find(|divider| contains(divider.area, event.column, event.row))
                {
                    self.dragging = Some(DragTarget {
                        pane_id: divider.pane_id.clone(),
                        axis: divider.axis,
                    });
                    return UiCommand::None;
                }
                let pane = self
                    .pane_areas
                    .iter()
                    .find(|(_, area)| contains(**area, event.column, event.row))
                    .map(|(id, _)| id.clone());
                let Some((workspace_id, pane_id)) = self
                    .workspace()
                    .and_then(|w| pane.map(|p| (w.workspace_id.clone(), p)))
                else {
                    return UiCommand::None;
                };
                // A press inside a pane body only *arms* a selection; copy mode is entered
                // on the first drag. Entering it here would mean every click that focuses a
                // pane also puts the keyboard into a mode that swallows it.
                let armed = self
                    .workspace()
                    .and_then(|workspace| workspace.panes.get(&pane_id))
                    .and_then(|pane| pane.run_id.clone())
                    .zip(self.pane_inner_areas.get(&pane_id).copied())
                    .and_then(|(run_id, inner)| {
                        grid_cell(inner, event.column, event.row).map(|origin| PaneDrag {
                            run_id,
                            origin,
                            inner,
                        })
                    });
                self.pane_drag = armed;
                self.layout.workspaces[self.workspace_index].focused_pane_id = pane_id.clone();
                UiCommand::Request(Box::new(Request::Workspace(WorkspaceRequest::Focus {
                    workspace_id,
                    pane_id,
                })))
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                // A press landed either on a divider or in a pane body, never both, so this
                // reads the pane gesture first and leaves the divider path below untouched.
                if let Some(drag) = self.pane_drag.clone() {
                    self.drag_selection(&drag, event.column, event.row);
                    return UiCommand::None;
                }
                let Some(target) = self.dragging.as_ref() else {
                    return UiCommand::None;
                };
                let Some(divider) = self.dividers.iter().find(|divider| {
                    divider.pane_id == target.pane_id && divider.axis == target.axis
                }) else {
                    self.dragging = None;
                    return UiCommand::None;
                };
                let ratio = drag_ratio(divider, event.column, event.row);
                let pane_id = divider.pane_id.clone();
                let Some(workspace) = self.workspace() else {
                    return UiCommand::None;
                };
                let workspace_id = workspace.workspace_id.clone();
                set_parent_ratio(
                    &mut self.layout.workspaces[self.workspace_index].root,
                    &pane_id,
                    ratio,
                );
                UiCommand::Request(Box::new(Request::Workspace(WorkspaceRequest::Resize {
                    workspace_id,
                    pane_id,
                    ratio_milli: ratio,
                })))
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.dragging = None;
                // The selection stands, and nothing is copied. Yank is always an explicit
                // `y`, so a stray drag can never overwrite what the user copied earlier.
                self.pane_drag = None;
                UiCommand::None
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                // Three rows per notch matches what terminals send for a single wheel click.
                let delta = if event.kind == MouseEventKind::ScrollUp {
                    3
                } else {
                    -3
                };
                let run_id = self
                    .pane_areas
                    .iter()
                    .find(|(_, area)| contains(**area, event.column, event.row))
                    .and_then(|(pane_id, _)| self.workspace()?.panes.get(pane_id))
                    .and_then(|pane| pane.run_id.clone());
                if let Some(screen) = run_id.and_then(|id| self.screens.get_mut(&id)) {
                    screen.scroll_by(delta);
                }
                UiCommand::None
            }
            _ => UiCommand::None,
        }
    }

    fn next_unique_id(&mut self, prefix: &str) -> String {
        self.sequence = self.sequence.max(
            self.layout
                .workspaces
                .iter()
                .flat_map(|workspace| {
                    std::iter::once(workspace.workspace_id.as_str())
                        .chain(workspace.panes.keys().map(String::as_str))
                })
                .filter_map(|id| id.rsplit_once('_')?.1.parse::<u64>().ok())
                .max()
                .unwrap_or(0),
        );
        loop {
            self.sequence = self
                .sequence
                .checked_add(1)
                .expect("generated ID space exhausted");
            let candidate = format!("{prefix}_{}", self.sequence);
            let collision = self.layout.workspaces.iter().any(|workspace| {
                workspace.workspace_id == candidate || workspace.panes.contains_key(&candidate)
            });
            if !collision {
                return candidate;
            }
        }
    }
}

fn matching_profiles(query: &str) -> impl Iterator<Item = usize> + '_ {
    (0..PROFILES.len()).filter(move |index| profile_matches(*index, query))
}

fn profile_matches(index: usize, query: &str) -> bool {
    PROFILES[index]
        .1
        .to_ascii_lowercase()
        .contains(&query.to_ascii_lowercase())
}

fn next_matching(current: usize, query: &str) -> usize {
    (1..=PROFILES.len())
        .map(|offset| (current + offset) % PROFILES.len())
        .find(|index| profile_matches(*index, query))
        .unwrap_or(current)
}

fn previous_matching(current: usize, query: &str) -> usize {
    (1..=PROFILES.len())
        .map(|offset| (current + PROFILES.len() - offset) % PROFILES.len())
        .find(|index| profile_matches(*index, query))
        .unwrap_or(current)
}

fn split_leaf(node: &mut LayoutNode, pane_id: &str, new_pane_id: String, axis: SplitAxis) -> bool {
    match node {
        LayoutNode::Pane { pane_id: id } if id == pane_id => {
            let old = id.clone();
            *node = LayoutNode::Split {
                axis,
                ratio_milli: 500,
                first: Box::new(LayoutNode::Pane { pane_id: old }),
                second: Box::new(LayoutNode::Pane {
                    pane_id: new_pane_id,
                }),
            };
            true
        }
        LayoutNode::Pane { .. } => false,
        LayoutNode::Split { first, second, .. } => {
            split_leaf(first, pane_id, new_pane_id.clone(), axis)
                || split_leaf(second, pane_id, new_pane_id, axis)
        }
    }
}

fn adjust_parent_ratio(node: &mut LayoutNode, pane_id: &str, delta: i16) -> Option<u16> {
    match node {
        LayoutNode::Pane { .. } => None,
        LayoutNode::Split {
            ratio_milli,
            first,
            second,
            ..
        } => {
            if first_leaf(second) == pane_id {
                *ratio_milli = (i32::from(*ratio_milli) + i32::from(delta)).clamp(100, 900) as u16;
                Some(*ratio_milli)
            } else {
                adjust_parent_ratio(first, pane_id, delta)
                    .or_else(|| adjust_parent_ratio(second, pane_id, delta))
            }
        }
    }
}

fn set_parent_ratio(node: &mut LayoutNode, pane_id: &str, ratio: u16) -> bool {
    match node {
        LayoutNode::Pane { .. } => false,
        LayoutNode::Split {
            ratio_milli,
            first,
            second,
            ..
        } => {
            if first_leaf(second) == pane_id {
                *ratio_milli = ratio;
                true
            } else {
                set_parent_ratio(first, pane_id, ratio) || set_parent_ratio(second, pane_id, ratio)
            }
        }
    }
}

fn split_rect(area: Rect, axis: SplitAxis, ratio: u16) -> (Rect, Rect, Rect) {
    match axis {
        SplitAxis::Vertical => {
            let available = area.width.saturating_sub(1);
            let first = ((u32::from(available) * u32::from(ratio)) / 1000) as u16;
            (
                Rect::new(area.x, area.y, first, area.height),
                Rect::new(area.x + first, area.y, 1, area.height),
                Rect::new(area.x + first + 1, area.y, available - first, area.height),
            )
        }
        SplitAxis::Horizontal => {
            let available = area.height.saturating_sub(1);
            let first = ((u32::from(available) * u32::from(ratio)) / 1000) as u16;
            (
                Rect::new(area.x, area.y, area.width, first),
                Rect::new(area.x, area.y + first, area.width, 1),
                Rect::new(area.x, area.y + first + 1, area.width, available - first),
            )
        }
    }
}

fn first_leaf(node: &LayoutNode) -> &str {
    match node {
        LayoutNode::Pane { pane_id } => pane_id,
        LayoutNode::Split { first, .. } => first_leaf(first),
    }
}
/// The one-row rectangle for the sidebar line at `index`, or `None` when that line falls past
/// the bottom of the sidebar. A rectangle recorded off-screen would claim pointer coordinates
/// belonging to whatever is drawn there instead, so a row that was never rendered is not
/// clickable.
fn clickable_row(area: Rect, index: usize) -> Option<Rect> {
    let row = area.y.checked_add(u16::try_from(index).ok()?)?;
    (row < area.bottom()).then(|| Rect::new(area.x, row, area.width, 1))
}

/// The cell of a pane's grid under the pointer, or `None` if the pointer is on the border
/// or outside the pane entirely.
fn grid_cell(inner: Rect, column: u16, row: u16) -> Option<(u16, u16)> {
    contains(inner, column, row).then(|| (row - inner.y, column - inner.x))
}

/// The same conversion for a pointer that has been dragged past the pane's edge, clamped to
/// the nearest cell so the selection keeps growing instead of freezing at the boundary.
fn clamp_cell(inner: Rect, column: u16, row: u16) -> (u16, u16) {
    let last_row = inner.bottom().saturating_sub(1).max(inner.y);
    let last_column = inner.right().saturating_sub(1).max(inner.x);
    (
        row.clamp(inner.y, last_row) - inner.y,
        column.clamp(inner.x, last_column) - inner.x,
    )
}

/// A cell of the frame buffer addressed in a pane's grid coordinates.
fn cell_at(buffer: &mut Buffer, inner: Rect, row: u16, column: u16) -> Option<&mut Cell> {
    if row >= inner.height || column >= inner.width {
        return None;
    }
    buffer.cell_mut((inner.x + column, inner.y + row))
}

fn contains(area: Rect, x: u16, y: u16) -> bool {
    x >= area.x && x < area.right() && y >= area.y && y < area.bottom()
}
fn drag_ratio(divider: &Divider, x: u16, y: u16) -> u16 {
    let (position, length, minimum) = match divider.axis {
        SplitAxis::Vertical => (
            x.saturating_sub(divider.container.x),
            divider.container.width.saturating_sub(1),
            MIN_PANE_WIDTH,
        ),
        SplitAxis::Horizontal => (
            y.saturating_sub(divider.container.y),
            divider.container.height.saturating_sub(1),
            MIN_PANE_HEIGHT,
        ),
    };
    let low = minimum.min(length / 2);
    let bounded = position.clamp(low, length.saturating_sub(low));
    if length == 0 {
        500
    } else {
        ((u32::from(bounded) * 1000) / u32::from(length)) as u16
    }
}
fn ellipsise(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_owned();
    }
    value
        .chars()
        .take(width.saturating_sub(1))
        .chain(std::iter::once('…'))
        .collect()
}

fn runtime_label(runtime: PaneRuntime) -> &'static str {
    match runtime {
        PaneRuntime::Running => "running",
        PaneRuntime::Exited => "exited",
        PaneRuntime::Restored => "restored",
        PaneRuntime::Empty => "empty",
    }
}

/// Whether a key carries a composing modifier, i.e. it is somebody reaching past the current
/// mode rather than pressing one of its letters. Shift is excluded on purpose: crossterm
/// reports an uppercase `G` or `N` with Shift set, and those are real copy-mode bindings.
fn composed(key: KeyEvent) -> bool {
    key.modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::PaneLayout;
    use crate::{
        adapter::{AdapterCapabilities, ProcessCapabilities},
        protocol::{ProcessState, ProviderState},
    };
    use crossterm::event::KeyEventKind;
    use ratatui::{Terminal, backend::TestBackend};
    use std::collections::BTreeMap;

    fn dashboard() -> Dashboard {
        let panes = BTreeMap::from([
            (
                "a".into(),
                PaneLayout {
                    pane_id: "a".into(),
                    name: "editor".into(),
                    run_id: None,
                    runtime: PaneRuntime::Running,
                },
            ),
            (
                "b".into(),
                PaneLayout {
                    pane_id: "b".into(),
                    name: "agent".into(),
                    run_id: None,
                    runtime: PaneRuntime::Restored,
                },
            ),
        ]);
        Dashboard {
            layout: LayoutSnapshot {
                workspaces: vec![WorkspaceLayout {
                    workspace_id: "w".into(),
                    name: "Daily".into(),
                    focused_pane_id: "a".into(),
                    panes,
                    root: LayoutNode::Split {
                        axis: SplitAxis::Vertical,
                        ratio_milli: 500,
                        first: Box::new(LayoutNode::Pane {
                            pane_id: "a".into(),
                        }),
                        second: Box::new(LayoutNode::Pane {
                            pane_id: "b".into(),
                        }),
                    },
                }],
            },
            ..Dashboard::default()
        }
    }

    fn prefix(dashboard: &mut Dashboard) {
        assert_eq!(
            dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)),
            UiCommand::None
        );
        assert!(dashboard.prefix_pending());
    }

    fn command(dashboard: &mut Dashboard, code: KeyCode) -> UiCommand {
        prefix(dashboard);
        dashboard.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn render_to_string(dashboard: &mut Dashboard, width: u16, height: u16) -> String {
        rendered(&render_terminal(dashboard, width, height))
    }

    /// The whole frame as text, which is all most tests need. Style is deliberately dropped
    /// here — anything asserting on colour must read the buffer, not this string.
    fn rendered(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn render_terminal(
        dashboard: &mut Dashboard,
        width: u16,
        height: u16,
    ) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| dashboard.render(frame)).unwrap();
        terminal
    }

    /// One row of the buffer, bounded to a rect's columns. The pane title lives on the
    /// border row, so a whole-frame string cannot tell it apart from the footer.
    fn row_text(terminal: &Terminal<TestBackend>, area: Rect, row: u16) -> String {
        let buffer = terminal.backend().buffer();
        (area.x..area.right())
            .map(|column| buffer[(column, row)].symbol())
            .collect()
    }

    /// Every cell of a pane's body carrying `background`, in grid coordinates.
    fn cells_with_background(
        terminal: &Terminal<TestBackend>,
        area: Rect,
        background: ratatui::style::Color,
    ) -> Vec<(u16, u16)> {
        let buffer = terminal.backend().buffer();
        let mut found = Vec::new();
        for row in area.y + 1..area.bottom() - 1 {
            for column in area.x + 1..area.right() - 1 {
                if buffer[(column, row)].bg == background {
                    found.push((row - area.y - 1, column - area.x - 1));
                }
            }
        }
        found
    }

    /// A full-screen seed for `run_id`, sized to the inner geometry a 100x30 dashboard gives
    /// the left pane, so nothing this feeds is clipped for a reason other than pane size.
    fn attach_event(run_id: &str, bytes: &[u8]) -> Event {
        let mut source = crate::terminal::VtTerminal::new(PANE_ROWS, PANE_COLS, 0);
        source.feed(bytes);
        Event::PaneAttached {
            run_id: run_id.into(),
            revision: 1,
            rows: PANE_ROWS,
            cols: PANE_COLS,
            screen: STANDARD.encode(source.state_bytes()),
        }
    }

    /// Inner geometry of pane "a" when the fixture dashboard is drawn at 100x30: a two-row
    /// header and a two-row footer leave a 26-row body; the 28-column sidebar leaves 72
    /// columns, whose even vertical split gives the left pane 35; borders take one cell on
    /// each side of both axes.
    const PANE_ROWS: u16 = 24;
    const PANE_COLS: u16 = 33;

    /// A second, single-pane workspace so switching has somewhere to go. Its pane is bound so
    /// the switch has a PTY whose geometry must be announced.
    fn two_workspace_dashboard() -> Dashboard {
        let mut dashboard = bound_dashboard();
        dashboard.layout.workspaces.push(WorkspaceLayout {
            workspace_id: "w2".into(),
            name: "Deploy".into(),
            focused_pane_id: "c".into(),
            panes: BTreeMap::from([(
                "c".into(),
                PaneLayout {
                    pane_id: "c".into(),
                    name: "deploy".into(),
                    run_id: Some("run_2".into()),
                    runtime: PaneRuntime::Running,
                },
            )]),
            root: LayoutNode::Pane {
                pane_id: "c".into(),
            },
        });
        dashboard
    }

    fn bound_dashboard() -> Dashboard {
        let mut dashboard = dashboard();
        dashboard.layout.workspaces[0]
            .panes
            .get_mut("a")
            .unwrap()
            .run_id = Some("run_1".into());
        dashboard
    }

    fn snapshot() -> RuntimeSnapshot {
        RuntimeSnapshot {
            binding_kind: crate::protocol::BindingKind::Repository,
            repository_root: "/repo/real".into(),
            external_task_ref: "TASK-61".into(),
            run_id: "dock_real".into(),
            worktree: "/repo/real".into(),
            branch: "main".into(),
            base_sha: "abc".into(),
            workspace_id: "w".into(),
            pane_id: "a".into(),
            state: ProcessState::Running,
            pid: Some(1),
            process_group_id: Some(1),
            command: vec!["sh".into()],
            adapter: AdapterId::Fixture,
            process_capabilities: ProcessCapabilities::OWNED_RUNTIME,
            adapter_capabilities: AdapterCapabilities::NONE,
            provider_state: ProviderState::Running,
            rows: 24,
            cols: 80,
            agent: None,
            agent_state: crate::detect::AgentState::Idle,
            title: None,
            cwd: None,
            diagnostic: None,
        }
    }

    #[test]
    fn a_paste_reaches_the_pane_as_one_bracketed_payload_with_a_single_trailing_terminator() {
        let mut dashboard = bound_dashboard();
        // The pane's program turns bracketed paste on; the client reads the mode off its own
        // replica of that pane rather than assuming it.
        dashboard.apply_event(attach_event("run_1", b"\x1b[?2004h"));
        let payload = "first\nsecond\x1b[201~rm -rf /\nthird";
        let UiCommand::PaneInput(bytes) = dashboard.paste(payload.into()) else {
            panic!("a paste into a live pane must become pane input");
        };
        assert!(bytes.starts_with(b"\x1b[200~"), "{bytes:?}");
        assert!(bytes.ends_with(b"\x1b[201~"), "{bytes:?}");
        let terminators = bytes
            .windows(6)
            .filter(|window| *window == b"\x1b[201~")
            .count();
        assert_eq!(terminators, 1, "smuggled terminator survived: {bytes:?}");
        // One input carrying every line, so nothing executes as it lands.
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("first") && text.contains("third"), "{text:?}");
    }

    #[test]
    fn a_paste_into_a_pane_that_never_enabled_the_mode_is_not_wrapped() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event("run_1", b"plain"));
        assert_eq!(
            dashboard.paste("hi".into()),
            UiCommand::PaneInput(b"hi".to_vec())
        );
    }

    #[test]
    fn an_exited_pane_is_visibly_exited_and_recoverable_from_the_keyboard() {
        let mut dashboard = bound_dashboard();
        dashboard.layout.workspaces[0]
            .panes
            .get_mut("a")
            .unwrap()
            .runtime = PaneRuntime::Exited;
        let text = render_to_string(&mut dashboard, 110, 28);
        assert!(text.contains("exited"), "{text:?}");
        assert!(text.contains("Ctrl+B R restarts"), "{text:?}");
        // Typing must not vanish into a pane with no process behind it.
        assert_eq!(
            dashboard.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            UiCommand::None
        );
        assert!(
            dashboard
                .error
                .as_deref()
                .is_some_and(|error| error.contains("Ctrl+B R")),
            "{:?}",
            dashboard.error
        );
        assert_eq!(dashboard.paste("still dropped".into()), UiCommand::None);
        assert!(matches!(
            command(&mut dashboard, KeyCode::Char('R')),
            UiCommand::Request(request)
                if matches!(request.as_ref(), Request::Workspace(WorkspaceRequest::Respawn { workspace_id, pane_id })
                    if workspace_id == "w" && pane_id == "a")
        ));
    }

    #[test]
    fn the_agent_roster_drops_entries_whose_run_is_gone() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(Event::AgentStateChanged {
            run_id: "run_1".into(),
            agent: Some(AgentKind::Claude),
            state: AgentState::Blocked,
        });
        dashboard.apply_event(Event::AgentStateChanged {
            run_id: "dock_sh_w_a".into(),
            agent: None,
            state: AgentState::Idle,
        });
        assert_eq!(dashboard.agents.len(), 2);
        let mut live = snapshot();
        live.run_id = "run_1".into();
        dashboard.set_runs(vec![live]);
        // The retired shell is gone from the roster; nothing else removed it before.
        assert_eq!(dashboard.agents.len(), 1);
        assert!(dashboard.agents.contains_key("run_1"));
        // A re-established event stream re-attaches every live run from scratch, so the roster
        // must be dropped with the screens rather than painting rows for runs that never return.
        dashboard.detach_screens();
        assert!(dashboard.agents.is_empty());
    }

    #[test]
    fn renders_split_focus_states_and_narrow_fallback() {
        for (width, height) in [(90, 24), (40, 10)] {
            let mut dashboard = dashboard();
            let text = render_to_string(&mut dashboard, width, height);
            assert!(text.contains("Daily"));
            assert!(text.contains(if width < 52 { "compact" } else { "editor" }));
        }
        // Focus is now carried entirely by the theme's border tokens rather than by a
        // per-runtime body colour, so that is what the render has to prove.
        let mut dashboard = dashboard();
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal.draw(|frame| dashboard.render(frame)).unwrap();
        let theme = Theme::warm();
        let focused = dashboard.pane_areas["a"];
        let unfocused = dashboard.pane_areas["b"];
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(focused.x, focused.y + 1)].fg, theme.border_focused);
        assert_eq!(buffer[(unfocused.x, unfocused.y + 1)].fg, theme.border);
        assert_ne!(theme.border_focused, theme.border);
    }

    #[test]
    fn keyboard_and_mouse_focus_and_bounded_resize() {
        let mut dashboard = dashboard();
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal.draw(|frame| dashboard.render(frame)).unwrap();
        prefix(&mut dashboard);
        let tab = dashboard.key(KeyEvent::new_with_kind(
            KeyCode::Tab,
            KeyModifiers::NONE,
            KeyEventKind::Press,
        ));
        assert!(
            matches!(tab, UiCommand::Request(request) if matches!(request.as_ref(), Request::Workspace(WorkspaceRequest::Focus { pane_id, .. }) if pane_id == "b"))
        );
        let b = dashboard.pane_areas["b"];
        let focus = dashboard.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: b.x + 1,
            row: b.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        assert!(
            matches!(focus, UiCommand::Request(request) if matches!(request.as_ref(), Request::Workspace(WorkspaceRequest::Focus { pane_id, .. }) if pane_id == "b"))
        );
        let divider = dashboard.dividers[0].area;
        dashboard.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: divider.x,
            row: divider.y,
            modifiers: KeyModifiers::NONE,
        });
        let resize = dashboard.mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 0,
            row: divider.y,
            modifiers: KeyModifiers::NONE,
        });
        assert!(
            matches!(resize, UiCommand::Request(request) if matches!(request.as_ref(), Request::Workspace(WorkspaceRequest::Resize { ratio_milli, .. }) if *ratio_milli > 0 && *ratio_milli < 500))
        );
    }

    #[test]
    fn resize_to_narrow_during_drag_clears_stale_divider_safely() {
        let mut dashboard = dashboard();
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal.draw(|frame| dashboard.render(frame)).unwrap();
        let divider = dashboard.dividers[0].area;
        dashboard.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: divider.x,
            row: divider.y,
            modifiers: KeyModifiers::NONE,
        });
        assert!(dashboard.dragging.is_some());
        terminal.backend_mut().resize(40, 10);
        terminal.draw(|frame| dashboard.render(frame)).unwrap();
        assert!(dashboard.dragging.is_none());
        assert_eq!(
            dashboard.mouse(MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                column: 1,
                row: 1,
                modifiers: KeyModifiers::NONE,
            }),
            UiCommand::None
        );
    }

    #[test]
    fn generated_ids_skip_ids_restored_from_persisted_snapshot() {
        let mut dashboard = dashboard();
        dashboard.layout.workspaces[0].workspace_id = "workspace_1".into();
        dashboard.layout.workspaces[0].panes.insert(
            "workspace_2".into(),
            PaneLayout {
                pane_id: "workspace_2".into(),
                name: "collision".into(),
                run_id: None,
                runtime: PaneRuntime::Restored,
            },
        );
        dashboard.layout.workspaces[0].panes.insert(
            "pane_3".into(),
            PaneLayout {
                pane_id: "pane_3".into(),
                name: "persisted".into(),
                run_id: None,
                runtime: PaneRuntime::Restored,
            },
        );
        let create = command(&mut dashboard, KeyCode::Char('n'));
        assert!(matches!(
            create,
            UiCommand::Request(request)
                if matches!(request.as_ref(), Request::Workspace(WorkspaceRequest::Create { workspace_id, pane_id, .. })
                    if workspace_id == "workspace_4" && pane_id == "pane_5")
        ));
        let split = command(&mut dashboard, KeyCode::Char('h'));
        assert!(matches!(
            split,
            UiCommand::Request(request)
                if matches!(request.as_ref(), Request::Workspace(WorkspaceRequest::Split { new_pane_id, .. })
                    if new_pane_id == "pane_6")
        ));
    }

    #[test]
    fn binding_facts_move_into_the_pane_title_and_never_replace_the_screen() {
        let mut dashboard = dashboard();
        dashboard.layout.workspaces[0]
            .panes
            .get_mut("a")
            .unwrap()
            .run_id = Some("dock_real".into());
        dashboard.runs.push(snapshot());
        let text = render_to_string(&mut dashboard, 110, 28);
        // The body is the emulated screen now, so the run's identity has to survive in the
        // one place still reserved for facts about the binding: the pane's own title.
        assert!(text.contains("dock_real · TASK-61"), "{text:?}");
        assert!(text.contains("agent · unbound"), "{text:?}");
        for gone in [
            "repository: /repo/real",
            "task: TASK-61",
            "binding: w/a",
            "No Dock-owned run bound",
        ] {
            assert!(!text.contains(gone), "pane body still prints {gone}");
        }
    }

    #[test]
    fn external_dismiss_and_owned_launch_have_keyboard_and_mouse_actions() {
        let mut dashboard = dashboard();
        dashboard.repository_root = "/repo".into();
        dashboard.runtime_directory = "/tmp".into();
        dashboard.external.push(ExternalAgentCandidate {
            provider: "Codex CLI".into(),
            repository_match: false,
        });
        // `d` is a pane keystroke now, so it must never clear the list.
        assert!(!matches!(
            dashboard.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)),
            UiCommand::Request(_)
        ));
        assert_eq!(dashboard.external.len(), 1);
        dashboard.external.clear();
        assert_eq!(
            command(&mut dashboard, KeyCode::Char('l')),
            UiCommand::LoadCatalog
        );
        assert_eq!(
            dashboard.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            UiCommand::None
        );
        assert!(
            matches!(dashboard.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), UiCommand::Request(request)
            if matches!(request.as_ref(), Request::TerminalLaunch(request) if request.profile == DashboardProfile::Fixture && request.runtime_directory == "/tmp" && request.workspace_id == "w" && request.pane_id == "a"))
        );

        dashboard.external.push(ExternalAgentCandidate {
            provider: "Claude Code".into(),
            repository_match: false,
        });
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal.draw(|frame| dashboard.render(frame)).unwrap();
        let dismiss = dashboard.dismiss_external_area.unwrap();
        assert_eq!(
            dashboard.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: dismiss.x + 1,
                row: dismiss.y,
                modifiers: KeyModifiers::NONE
            }),
            UiCommand::None
        );
        assert!(dashboard.external.is_empty());
        terminal.draw(|frame| dashboard.render(frame)).unwrap();
        let launch = dashboard.launch_area.unwrap();
        assert_eq!(
            dashboard.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: launch.x + 1,
                row: launch.y,
                modifiers: KeyModifiers::NONE
            }),
            UiCommand::LoadCatalog
        );
        assert!(dashboard.launch_form.is_some());
        assert_eq!(
            dashboard.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            UiCommand::None
        );
        assert!(dashboard.launch_form.is_none());
    }

    #[test]
    fn mouse_launch_form_selects_reviews_and_confirms_the_exact_focused_pane() {
        let mut dashboard = dashboard();
        dashboard.runtime_directory = "/tmp".into();
        dashboard.open_launch();
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal.draw(|frame| dashboard.render(frame)).unwrap();
        let profile = dashboard.launch_profile_areas[0];
        assert_eq!(
            dashboard.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: profile.x,
                row: profile.y,
                modifiers: KeyModifiers::NONE
            }),
            UiCommand::None
        );
        let confirm = dashboard.launch_confirm_area.unwrap();
        assert_eq!(
            dashboard.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: confirm.x,
                row: confirm.y,
                modifiers: KeyModifiers::NONE
            }),
            UiCommand::None
        );
        assert!(
            matches!(dashboard.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: confirm.x, row: confirm.y, modifiers: KeyModifiers::NONE }), UiCommand::Request(request)
            if matches!(request.as_ref(), Request::TerminalLaunch(request) if request.workspace_id == "w" && request.pane_id == "a"))
        );
    }

    #[test]
    fn repository_mode_constructs_only_the_existing_verified_option() {
        let mut dashboard = dashboard();
        dashboard.repository_root = "/repo".into();
        dashboard.repository_launches.push(RepositoryLaunchOption {
            task_ref: "TASK-12".into(),
            worktree: "/repo/wt".into(),
        });
        dashboard.open_launch();
        dashboard.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        dashboard.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            matches!(dashboard.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), UiCommand::Request(request)
            if matches!(request.as_ref(), Request::LaunchIntoPane(request) if request.workspace_id == "w" && request.pane_id == "a" && request.dispatch.external_task_ref == "TASK-12" && request.dispatch.worktree == "/repo/wt"))
        );
        assert!(
            PROFILES
                .iter()
                .all(|(profile, _)| AdapterId::from(*profile) != AdapterId::Generic)
        );
    }

    #[test]
    fn published_keymap_help_is_contextual_and_escape_is_local() {
        let mut dashboard = dashboard();
        assert_eq!(command(&mut dashboard, KeyCode::Char('?')), UiCommand::None);
        assert!(dashboard.help_open);
        let text = render_to_string(&mut dashboard, 90, 24);
        for key in [
            "Every key goes to the focused pane",
            "n new workspace",
            "h/v split",
            "z zoom",
            "r rename",
            "x close",
            "l launch",
            "q quit",
            "runs keep running",
        ] {
            assert!(text.contains(key), "missing published mnemonic: {key}");
        }
        // Esc closes the overlay instead of reaching the pane, which is the one place the
        // dashboard is still allowed to keep a key for itself.
        assert_eq!(
            dashboard.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            UiCommand::None
        );
        assert!(!dashboard.help_open);
    }

    #[test]
    fn every_pane_key_is_fire_and_forget_input_and_never_a_daemon_request() {
        let mut dashboard = bound_dashboard();
        dashboard.runs.push(snapshot());
        // There is no mode to enter any more: the very first keystroke is pane input, and it
        // must be `PaneInput`, because the `Request` arm costs two daemon round trips before
        // the echo can be painted.
        for (key, expected) in [
            (
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
                b"q".to_vec(),
            ),
            (KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), vec![0x1b]),
            (
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                b"\r".to_vec(),
            ),
            (
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                vec![0x03],
            ),
        ] {
            let outcome = dashboard.key(key);
            assert_eq!(
                outcome,
                UiCommand::PaneInput(expected),
                "{key:?} must be fire-and-forget pane input"
            );
            assert!(
                !matches!(outcome, UiCommand::Request(_)),
                "{key:?} took the slow request path"
            );
        }
        // Only the prefixed form still commands the dashboard.
        assert_eq!(command(&mut dashboard, KeyCode::Char('q')), UiCommand::Quit);
        assert_eq!(command(&mut dashboard, KeyCode::Char('d')), UiCommand::Quit);
    }

    #[test]
    fn launch_typeahead_review_and_safe_choice_retention_are_pointer_independent() {
        let mut dashboard = dashboard();
        dashboard.runtime_directory = "/tmp".into();
        dashboard.open_launch();
        for character in "fix".chars() {
            assert_eq!(
                dashboard.key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)),
                UiCommand::None
            );
        }
        assert_eq!(dashboard.launch_form.as_ref().unwrap().index, 0);
        assert_eq!(
            dashboard.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            UiCommand::None
        );
        assert!(dashboard.launch_form.as_ref().unwrap().confirming);
        assert!(
            matches!(dashboard.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), UiCommand::Request(request) if matches!(request.as_ref(), Request::TerminalLaunch(request) if request.profile == DashboardProfile::Fixture))
        );
        dashboard.open_launch();
        let retained = dashboard.launch_form.as_ref().unwrap();
        assert_eq!(retained.index, 0);
        assert!(!retained.repository_mode);
        assert!(retained.query.is_empty());
    }

    #[test]
    fn focus_split_resize_and_forms_change_locally_before_requests_complete() {
        let mut dashboard = dashboard();
        let focus = command(&mut dashboard, KeyCode::Tab);
        assert!(matches!(focus, UiCommand::Request(_)));
        assert_eq!(dashboard.workspace().unwrap().focused_pane_id, "b");
        prefix(&mut dashboard);
        let back = dashboard.key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert!(matches!(back, UiCommand::Request(_)));
        assert_eq!(dashboard.workspace().unwrap().focused_pane_id, "a");
        let focus = command(&mut dashboard, KeyCode::Tab);
        assert!(matches!(focus, UiCommand::Request(_)));
        let panes = dashboard.workspace().unwrap().panes.len();
        let split = command(&mut dashboard, KeyCode::Char('h'));
        assert!(matches!(split, UiCommand::Request(_)));
        assert_eq!(dashboard.workspace().unwrap().panes.len(), panes + 1);
        assert_eq!(command(&mut dashboard, KeyCode::Char('r')), UiCommand::None);
        assert!(dashboard.rename_form.is_some());
        assert_eq!(
            dashboard.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            UiCommand::None
        );
        assert!(dashboard.rename_form.is_none());
        assert_eq!(
            command(&mut dashboard, KeyCode::Char('l')),
            UiCommand::LoadCatalog
        );
        assert!(dashboard.launch_form.is_some());
        assert_eq!(
            dashboard.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            UiCommand::None
        );
        assert!(dashboard.launch_form.is_none());
    }

    #[test]
    fn unavailable_actions_always_explain_the_reason() {
        let mut dashboard = Dashboard::default();
        for key in ['h', 'r', 'x', '+', 'z'] {
            assert_eq!(command(&mut dashboard, KeyCode::Char(key)), UiCommand::None);
            assert!(
                dashboard
                    .error
                    .as_deref()
                    .is_some_and(|message| message.contains("unavailable")),
                "{key} silently no-op'd"
            );
        }
    }

    #[test]
    fn a_bound_pane_renders_emulated_screen_content_not_binding_facts() {
        let mut dashboard = bound_dashboard();
        // Twenty distinct rows, not one line: a screen with a couple of lines on it is
        // rendered identically whether the pane is 24 rows tall or 4, so only output that
        // fills the pane can prove the geometry as well as the content.
        let mut output = Vec::new();
        for index in 1..=20 {
            output.extend_from_slice(format!("screen row {index:02}\r\n").as_bytes());
        }
        dashboard.apply_event(attach_event("run_1", &output));
        let rendered = render_to_string(&mut dashboard, 100, 30);
        for index in 1..=20 {
            assert!(
                rendered.contains(&format!("screen row {index:02}")),
                "row {index} missing, so the pane is not being drawn at its full height"
            );
        }
        assert!(!rendered.contains("No Dock-owned run bound"));
    }

    #[test]
    fn an_unattached_pane_shows_a_placeholder_until_its_first_screen_arrives() {
        let mut dashboard = bound_dashboard();
        assert!(render_to_string(&mut dashboard, 100, 30).contains("starting…"));
        dashboard.apply_event(attach_event("run_1", b"shell is up\r\n"));
        let rendered = render_to_string(&mut dashboard, 100, 30);
        assert!(rendered.contains("shell is up"));
    }

    #[test]
    fn keys_reach_the_pane_and_the_prefix_opens_command_mode() {
        let mut dashboard = bound_dashboard();
        let outcome = dashboard.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(matches!(outcome, UiCommand::PaneInput(bytes) if bytes == b"x"));
        let pending = dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        assert_eq!(pending, UiCommand::None);
        assert!(dashboard.prefix_pending());
        // A second prefix is the escape hatch for a literal Ctrl+B in the pane.
        assert!(matches!(
            dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)),
            UiCommand::PaneInput(bytes) if bytes == vec![0x02]
        ));
        assert!(!dashboard.prefix_pending());
    }

    #[test]
    fn which_key_hints_appear_only_while_the_prefix_is_pending() {
        let mut dashboard = dashboard();
        let quiet = render_to_string(&mut dashboard, 100, 30);
        assert!(!quiet.contains("split"), "{quiet:?}");
        assert!(quiet.contains("Ctrl+B ? help"), "{quiet:?}");
        prefix(&mut dashboard);
        let pending = render_to_string(&mut dashboard, 100, 30);
        // "quit" is the last entry in the table, so asserting it proves the whole bar fits
        // inside the two-row footer rather than being silently clipped.
        for hint in [
            "split",
            "zoom",
            "runs keep running",
            "focus prev",
            "workspace",
            // The newest hint, and therefore the one nearest the point where the bar stops
            // fitting the two-row footer.
            "copy mode",
            "quit",
        ] {
            assert!(pending.contains(hint), "missing which-key hint {hint}");
        }
    }

    #[test]
    fn a_long_sidebar_label_is_shortened_rather_than_wrapped_onto_a_second_line() {
        let mut dashboard = dashboard();
        // Workspace names are the one piece of user-supplied text in the sidebar, so they are
        // what can outrun the 27 columns the right border leaves.
        dashboard.layout.workspaces[0].name = "release train for the whole fleet".into();
        let rows = sidebar_rows(&mut dashboard, 100, 30);
        assert!(
            rows.iter()
                .any(|row| row.contains("release train for the wh…")),
            "{rows:#?}"
        );
        assert!(
            !rows
                .iter()
                .any(|row| row.contains("release train for the whole fleet")),
            "{rows:#?}"
        );
    }

    #[test]
    fn sidebar_lists_agents_with_blocked_first() {
        let mut dashboard = dashboard();
        dashboard.apply_event(Event::AgentStateChanged {
            run_id: "run_idle".into(),
            agent: Some(AgentKind::Amp),
            state: AgentState::Idle,
        });
        dashboard.apply_event(Event::AgentStateChanged {
            run_id: "run_blocked".into(),
            agent: Some(AgentKind::Claude),
            state: AgentState::Blocked,
        });
        let rendered = render_to_string(&mut dashboard, 100, 30);
        let claude = rendered.find("claude").expect("claude listed");
        let amp = rendered.find("amp").expect("amp listed");
        assert!(claude < amp, "blocked agents must sort above idle ones");
    }

    #[test]
    fn a_bound_pane_is_resized_to_its_inner_geometry_and_only_when_it_changes() {
        let mut dashboard = bound_dashboard();
        render_to_string(&mut dashboard, 100, 30);
        let first = dashboard.take_pending_resizes();
        assert_eq!(
            first,
            vec![("w".to_owned(), "a".to_owned(), PANE_ROWS, PANE_COLS)],
            "only the pane with a run needs a PTY, and it must be told its exact inner size"
        );
        render_to_string(&mut dashboard, 100, 30);
        assert!(
            dashboard.take_pending_resizes().is_empty(),
            "unchanged geometry must not re-send"
        );
        render_to_string(&mut dashboard, 120, 40);
        assert_eq!(dashboard.take_pending_resizes().len(), 1);
    }

    #[test]
    fn zoom_gives_the_focused_pane_the_whole_body_and_resizes_its_pty() {
        let mut dashboard = bound_dashboard();
        render_to_string(&mut dashboard, 100, 30);
        dashboard.take_pending_resizes();
        assert_eq!(command(&mut dashboard, KeyCode::Char('z')), UiCommand::None);
        render_to_string(&mut dashboard, 100, 30);
        let zoomed = dashboard.take_pending_resizes();
        // The body is 72 columns wide; zoomed, pane "a" owns all of it rather than the 35
        // its half of the vertical split gave it.
        assert_eq!(
            zoomed,
            vec![("w".to_owned(), "a".to_owned(), PANE_ROWS, 70)]
        );
        assert!(
            !dashboard.pane_areas.contains_key("b"),
            "b is hidden while a is zoomed"
        );
        assert_eq!(command(&mut dashboard, KeyCode::Char('z')), UiCommand::None);
        render_to_string(&mut dashboard, 100, 30);
        assert_eq!(
            dashboard.take_pending_resizes(),
            vec![("w".to_owned(), "a".to_owned(), PANE_ROWS, PANE_COLS)]
        );
    }

    #[test]
    fn workspace_cycling_moves_the_rendered_workspace_and_saturates_at_both_ends() {
        let mut dashboard = two_workspace_dashboard();
        let first = render_to_string(&mut dashboard, 100, 30);
        assert!(first.contains("editor"));
        assert!(
            !first.contains("deploy"),
            "only the selected workspace is rendered"
        );
        assert_eq!(command(&mut dashboard, KeyCode::Char('.')), UiCommand::None);
        assert_eq!(dashboard.workspace_index, 1);
        let second = render_to_string(&mut dashboard, 100, 30);
        assert!(second.contains("Deploy"), "{second:?}");
        assert!(
            second.contains("deploy · run_2"),
            "the second workspace is not rendered"
        );
        assert!(
            !second.contains("editor"),
            "the first workspace is still on screen"
        );
        // Saturating rather than wrapping: `.` at the last workspace is a mis-press, and
        // silently jumping to the first would move the user somewhere they did not ask for.
        assert_eq!(command(&mut dashboard, KeyCode::Char('.')), UiCommand::None);
        assert_eq!(dashboard.workspace_index, 1);
        assert_eq!(command(&mut dashboard, KeyCode::Char(',')), UiCommand::None);
        assert_eq!(dashboard.workspace_index, 0);
        assert_eq!(command(&mut dashboard, KeyCode::Char(',')), UiCommand::None);
        assert_eq!(dashboard.workspace_index, 0);
        assert!(render_to_string(&mut dashboard, 100, 30).contains("editor"));
    }

    #[test]
    fn cycling_with_no_workspace_explains_itself_rather_than_panicking() {
        let mut dashboard = Dashboard::default();
        assert_eq!(command(&mut dashboard, KeyCode::Char('.')), UiCommand::None);
        assert_eq!(dashboard.workspace_index, 0);
        assert!(
            dashboard
                .error
                .as_deref()
                .is_some_and(|message| message.contains("unavailable"))
        );
    }

    #[test]
    fn a_workspace_switch_announces_the_newly_visible_pane_geometry() {
        let mut dashboard = two_workspace_dashboard();
        render_to_string(&mut dashboard, 100, 30);
        assert_eq!(
            dashboard.take_pending_resizes(),
            vec![("w".to_owned(), "a".to_owned(), PANE_ROWS, PANE_COLS)]
        );
        command(&mut dashboard, KeyCode::Char('.'));
        render_to_string(&mut dashboard, 100, 30);
        // The second workspace is a single pane, so it owns the whole 72-column body.
        assert_eq!(
            dashboard.take_pending_resizes(),
            vec![("w2".to_owned(), "c".to_owned(), PANE_ROWS, 70)],
            "a pane shown for the first time must be told its size"
        );
    }

    #[test]
    fn the_cursor_is_drawn_only_in_the_focused_pane() {
        let mut dashboard = bound_dashboard();
        dashboard.layout.workspaces[0]
            .panes
            .get_mut("b")
            .unwrap()
            .run_id = Some("run_2".into());
        // Both screens end on a newline, so each cursor sits on a blank cell and tui-term
        // draws its block symbol there rather than merely reversing an occupied cell.
        dashboard.apply_event(attach_event("run_1", b"left pane\r\n"));
        dashboard.apply_event(attach_event("run_2", b"right pane\r\n"));
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| dashboard.render(frame)).unwrap();
        let focused = dashboard.pane_areas["a"];
        let unfocused = dashboard.pane_areas["b"];
        assert_eq!(dashboard.workspace().unwrap().focused_pane_id, "a");
        assert!(
            draws_cursor(&terminal, focused),
            "the focused pane must show where typing lands"
        );
        assert!(
            !draws_cursor(&terminal, unfocused),
            "a cursor in every pane makes focus unreadable"
        );
    }

    fn draws_cursor(terminal: &Terminal<TestBackend>, area: Rect) -> bool {
        let buffer = terminal.backend().buffer();
        for y in area.y + 1..area.bottom() - 1 {
            for x in area.x + 1..area.right() - 1 {
                if buffer[(x, y)].symbol() == "\u{2588}" {
                    return true;
                }
            }
        }
        false
    }

    #[test]
    fn typing_into_a_pane_with_no_run_is_dropped_rather_than_sent() {
        let mut dashboard = dashboard();
        // Pane "a" has no run, so there is no PTY to receive this and the daemon would answer
        // every character with an error that flickers through the footer.
        assert_eq!(
            dashboard.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            UiCommand::None
        );
        assert!(dashboard.error.is_none(), "a dropped key must not shout");
        dashboard.layout.workspaces[0]
            .panes
            .get_mut("a")
            .unwrap()
            .run_id = Some("run_1".into());
        assert!(matches!(
            dashboard.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            UiCommand::PaneInput(bytes) if bytes == b"x"
        ));
    }

    #[test]
    fn attach_then_delta_events_reconstruct_the_pane_screen() {
        let mut dashboard = dashboard();
        let mut source = crate::terminal::VtTerminal::new(24, 80, 0);
        source.feed(b"first line\r\n");
        dashboard.apply_event(Event::PaneAttached {
            run_id: "run_1".into(),
            revision: 1,
            rows: 24,
            cols: 80,
            screen: STANDARD.encode(source.state_bytes()),
        });
        let mut sync = crate::terminal::ScreenSync::new(24, 80);
        sync.apply(&source.state_bytes());
        source.feed(b"second line\r\n");
        let delta = sync.delta_from(&source);
        dashboard.apply_event(Event::PaneDelta {
            run_id: "run_1".into(),
            revision: 2,
            bytes: STANDARD.encode(&delta),
        });
        let rendered = dashboard.screen_text("run_1").expect("screen present");
        assert!(rendered.contains("first line"), "{rendered:?}");
        assert!(rendered.contains("second line"), "{rendered:?}");
    }

    #[test]
    fn a_revision_gap_drops_the_screen_so_the_client_re_attaches() {
        let mut dashboard = dashboard();
        dashboard.apply_event(Event::PaneAttached {
            run_id: "run_1".into(),
            revision: 1,
            rows: 24,
            cols: 80,
            screen: String::new(),
        });
        dashboard.apply_event(Event::PaneDelta {
            run_id: "run_1".into(),
            revision: 9,
            bytes: String::new(),
        });
        assert!(dashboard.screen_text("run_1").is_none());
    }

    #[test]
    fn a_re_attach_for_a_known_run_rebuilds_the_parser_at_the_announced_geometry() {
        let mut dashboard = dashboard();
        dashboard.apply_event(Event::PaneAttached {
            run_id: "run_1".into(),
            revision: 1,
            rows: 24,
            cols: 80,
            screen: String::new(),
        });
        let mut source = crate::terminal::VtTerminal::new(10, 40, 0);
        source.feed(b"seed\r\n");
        dashboard.apply_event(Event::PaneAttached {
            run_id: "run_1".into(),
            revision: 7,
            rows: 10,
            cols: 40,
            screen: STANDARD.encode(source.state_bytes()),
        });
        assert_eq!(dashboard.screens["run_1"].size(), (10, 40));

        // Fifteen lines is more than the new ten-row screen holds, so a parser rebuilt at the
        // announced geometry scrolls the earliest ones off. One still sized twenty-four rows
        // would keep every line, which is what makes this distinguish geometry rather than
        // merely content: a shorter screen cannot be told from a taller one until the output
        // exceeds the shorter of the two heights.
        let mut lines = Vec::new();
        for index in 1..=15 {
            lines.extend_from_slice(format!("line {index:02}\r\n").as_bytes());
        }
        dashboard.apply_event(Event::PaneDelta {
            run_id: "run_1".into(),
            revision: 8,
            bytes: STANDARD.encode(&lines),
        });
        let rendered = dashboard.screen_text("run_1").expect("screen present");
        assert!(rendered.contains("line 15"), "{rendered:?}");
        assert!(
            !rendered.contains("line 01"),
            "a ten-row screen cannot still be holding the first of fifteen lines: {rendered:?}"
        );

        // The re-seed adopted the daemon's revision, which never restarts across a re-seed, so
        // the deltas above were contiguous rather than read as a gap and dropped.
        assert_eq!(dashboard.revisions.get("run_1"), Some(&8));
    }

    #[test]
    fn agent_state_events_are_recorded_and_layout_events_ask_for_one_refresh() {
        let mut dashboard = dashboard();
        dashboard.apply_event(Event::AgentStateChanged {
            run_id: "run_1".into(),
            agent: Some(crate::detect::AgentKind::Claude),
            state: crate::detect::AgentState::Working,
        });
        assert_eq!(
            dashboard.agents.get("run_1"),
            Some(&(
                Some(crate::detect::AgentKind::Claude),
                crate::detect::AgentState::Working
            ))
        );

        assert!(!dashboard.take_refresh());
        dashboard.apply_event(Event::LayoutChanged);
        assert!(dashboard.take_refresh());
        assert!(!dashboard.take_refresh(), "refresh must not latch on");
        assert!(dashboard.take_pending_resizes().is_empty());
    }

    /// Renders the sidebar and returns, for each row of `area`, the text actually painted there.
    fn sidebar_rows(dashboard: &mut Dashboard, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| dashboard.render(frame)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width.min(28))
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// Every clickable sidebar rectangle must sit on the row that actually carries its label.
    /// Recording rows from the logical line count while the paragraph wrapped meant a single
    /// long line pushed every rectangle below it off its own label, so the pointer hit the
    /// wrong control or nothing at all.
    #[test]
    fn sidebar_click_targets_land_on_the_rows_their_labels_are_rendered_on() {
        let mut dashboard = dashboard();
        // Longer than the 27 columns the sidebar's right border leaves, which is exactly what
        // used to wrap onto extra rows the recorded rectangles knew nothing about.
        dashboard.layout.workspaces[0].name = "a very long workspace name that overflows".into();
        dashboard.external.push(ExternalAgentCandidate {
            provider: "Codex CLI".into(),
            repository_match: false,
        });
        let rows = sidebar_rows(&mut dashboard, 100, 30);
        let dismiss = dashboard.dismiss_external_area.expect("dismiss row");
        let launch = dashboard.launch_area.expect("launch row");
        assert!(
            rows[usize::from(dismiss.y)].contains("click to dismiss all"),
            "dismiss rectangle at row {} but rows were {rows:#?}",
            dismiss.y
        );
        assert!(
            rows[usize::from(launch.y)].contains("LAUNCH AGENT"),
            "launch rectangle at row {} but rows were {rows:#?}",
            launch.y
        );
        // No sidebar row may carry a wrapped remainder of the workspace name: an over-long
        // label is truncated in place rather than stealing the row below it.
        assert_eq!(
            rows.iter().filter(|row| row.contains("long work")).count(),
            1,
            "{rows:#?}"
        );
        // And the rectangles still drive the real actions when clicked where they are drawn.
        assert_eq!(
            dashboard.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: dismiss.x + 1,
                row: dismiss.y,
                modifiers: KeyModifiers::NONE
            }),
            UiCommand::None
        );
        assert!(dashboard.external.is_empty());
        assert_eq!(
            dashboard.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: launch.x + 1,
                row: launch.y,
                modifiers: KeyModifiers::NONE
            }),
            UiCommand::LoadCatalog
        );
        assert!(dashboard.launch_form.is_some());
    }

    /// A sidebar with more entries than rows cannot record a rectangle for a control it never
    /// drew: those coordinates belong to whatever the terminal shows there instead.
    #[test]
    fn a_sidebar_control_pushed_past_the_last_row_records_no_click_target() {
        let mut dashboard = dashboard();
        for index in 0..40 {
            dashboard.apply_event(Event::AgentStateChanged {
                run_id: format!("run_{index}"),
                agent: Some(AgentKind::Claude),
                state: AgentState::Idle,
            });
        }
        let rows = sidebar_rows(&mut dashboard, 100, 30);
        assert!(
            !rows.iter().any(|row| row.contains("LAUNCH AGENT")),
            "{rows:#?}"
        );
        assert_eq!(dashboard.launch_area, None);
        // A click where the unrendered row would have been must not open the launch form.
        assert_eq!(
            dashboard.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 1,
                row: 29,
                modifiers: KeyModifiers::NONE
            }),
            UiCommand::None
        );
        assert!(dashboard.launch_form.is_none());
    }

    /// The roster lists agents. A pane's ambient shell is a run, not an agent, and used to be
    /// listed under its raw run id.
    #[test]
    fn the_agent_roster_lists_only_runs_whose_agent_was_detected() {
        let mut dashboard = dashboard();
        dashboard.apply_event(Event::AgentStateChanged {
            run_id: "dock_sh_workspace_1_pane_2".into(),
            agent: None,
            state: AgentState::Idle,
        });
        let rows = sidebar_rows(&mut dashboard, 100, 30);
        assert!(
            !rows.iter().any(|row| row.contains("dock_sh_")),
            "{rows:#?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("none running")),
            "{rows:#?}"
        );

        dashboard.apply_event(Event::AgentStateChanged {
            run_id: "run_1".into(),
            agent: Some(AgentKind::Claude),
            state: AgentState::Blocked,
        });
        assert_eq!(dashboard.agents.len(), 2);
        assert_eq!(
            dashboard.agent_roster(),
            vec![(AgentState::Blocked, AgentKind::Claude.label())]
        );
        let rows = sidebar_rows(&mut dashboard, 100, 30);
        assert!(
            !rows.iter().any(|row| row.contains("none running")),
            "{rows:#?}"
        );
        assert!(
            !rows.iter().any(|row| row.contains("dock_sh_")),
            "{rows:#?}"
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.contains(AgentKind::Claude.label()))
                .count(),
            1,
            "{rows:#?}"
        );
    }

    // CONTROLLER RULING C2: the brief's test used `dashboard()`, whose pane "a" has
    // `run_id: None`. Attaching a screen for "run_1" would leave the focused pane unbound and
    // the wheel event would land on empty space rather than exercising the scroll behaviour.
    // `bound_dashboard()` binds pane "a" to "run_1" so the pointer is actually over a screen.
    #[test]
    fn the_wheel_scrolls_the_pane_under_the_pointer_and_returning_to_live_resumes_following() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event("run_1", b""));
        if let Some(screen) = dashboard.screens.get_mut("run_1") {
            for index in 1..=60 {
                screen.feed(format!("line {index}\r\n").as_bytes());
            }
        }
        render_to_string(&mut dashboard, 100, 30);
        let area = *dashboard.pane_areas.get("a").expect("pane a is rendered");
        let (column, row) = (area.x + 2, area.y + 2);

        let scrolled = dashboard.mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(
            scrolled,
            UiCommand::None,
            "scrolling costs no daemon request"
        );
        assert!(
            dashboard.screens["run_1"].is_scrolled(),
            "the wheel must move the viewport into history"
        );

        for _ in 0..40 {
            dashboard.mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            });
        }
        assert!(
            !dashboard.screens["run_1"].is_scrolled(),
            "scrolling back to the bottom resumes following live output"
        );
    }

    // FINDING 2 (review of Task 4): pane "a" in `bound_dashboard()` is both focused and under
    // the pointer in the test above, so a handler that wrongly read `focused_pane_id` instead
    // of hit-testing `pane_areas` would pass it identically. Here pane "b" is bound to a second
    // run and the wheel lands on "b" while "a" stays focused, so only a pointer-pane
    // implementation scrolls the right screen.
    #[test]
    fn the_wheel_scrolls_the_pane_under_the_pointer_not_the_focused_pane() {
        let mut dashboard = bound_dashboard();
        dashboard.layout.workspaces[0]
            .panes
            .get_mut("b")
            .unwrap()
            .run_id = Some("run_2".into());
        assert_eq!(dashboard.layout.workspaces[0].focused_pane_id, "a");

        dashboard.apply_event(attach_event("run_1", b""));
        dashboard.apply_event(attach_event("run_2", b""));
        for run_id in ["run_1", "run_2"] {
            if let Some(screen) = dashboard.screens.get_mut(run_id) {
                for index in 1..=60 {
                    screen.feed(
                        format!(
                            "line {index}
"
                        )
                        .as_bytes(),
                    );
                }
            }
        }
        render_to_string(&mut dashboard, 100, 30);
        let area = *dashboard.pane_areas.get("b").expect("pane b is rendered");
        let (column, row) = (area.x + 2, area.y + 2);

        dashboard.mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        });

        assert!(
            dashboard.screens["run_2"].is_scrolled(),
            "the wheel must scroll the pane under the pointer (\"b\")"
        );
        assert!(
            !dashboard.screens["run_1"].is_scrolled(),
            "the wheel must not scroll the focused pane (\"a\") when the pointer is elsewhere"
        );
    }

    #[test]
    fn the_prefix_then_bracket_enters_copy_mode_and_escape_leaves_it() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event("run_1", b"hello world\r\n"));
        assert!(!dashboard.copy_mode());
        dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        dashboard.key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
        assert!(dashboard.copy_mode(), "Ctrl+B [ must enter copy mode");
        dashboard.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!dashboard.copy_mode(), "Esc always leaves copy mode");
    }

    #[test]
    fn copy_mode_keys_never_reach_the_pane() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event("run_1", b"hello world\r\n"));
        dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        dashboard.key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
        // Ctrl+C, Ctrl+D and Enter are the three that would actually do something to the
        // shell — interrupt it, close its stdin, run whatever is on the line. `y` is last
        // because yanking deliberately leaves the mode, and every key before it must find
        // copy mode still in charge for the loop to be proving containment at all.
        for key in [
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
        ] {
            assert!(
                dashboard.copy_mode(),
                "copy mode must still be active to be the thing swallowing {key:?}"
            );
            let outcome = dashboard.key(key);
            assert!(
                !matches!(outcome, UiCommand::PaneInput(_)),
                "{key:?} must not be forwarded to the PTY while in copy mode"
            );
        }
        assert!(!dashboard.copy_mode(), "the trailing y yanked and left");
    }

    #[test]
    fn a_bare_yank_copies_the_cursors_line_and_names_it() {
        let mut dashboard = bound_dashboard();
        // No trailing newline, so the live cursor — and therefore copy mode's — sits on the
        // row that actually holds the text.
        dashboard.apply_event(attach_event("run_1", b"https://example.test/path"));
        dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        dashboard.key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
        dashboard.key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(!dashboard.copy_mode(), "yanking leaves copy mode");
        let notice = dashboard.error.clone().unwrap_or_default();
        // 25 characters is the URL with the row's trailing blanks trimmed off, which is the
        // proof that the line — not the whole padded grid row — went to the clipboard.
        assert!(
            notice.contains("copied line 1 (25 characters)"),
            "a bare yank must copy the cursor's line and say so, got {notice:?}"
        );
    }

    #[test]
    fn escape_cancels_the_search_prompt_first_and_copy_mode_second() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event("run_1", b"needle in here"));
        dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        dashboard.key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
        dashboard.key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        dashboard.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(dashboard.copy_status().as_deref(), Some("COPY /n"));
        dashboard.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            dashboard.copy_mode(),
            "the first Esc cancels the prompt, not the mode"
        );
        assert_eq!(
            dashboard.copy_status().as_deref(),
            Some("COPY MOVE 1,15"),
            "the prompt is gone and the cursor is where it was"
        );
        dashboard.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!dashboard.copy_mode(), "the second Esc leaves copy mode");
    }

    #[test]
    fn control_modified_letters_are_not_copy_mode_bindings() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event("run_1", b"hello world"));
        dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        dashboard.key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
        let before = dashboard.copy_status();
        for key in ['h', 'j', 'v', 'g'] {
            dashboard.key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::CONTROL));
        }
        assert_eq!(
            dashboard.copy_status(),
            before,
            "Ctrl+letter is somebody reaching past copy mode, not a motion or a verb"
        );
        // Shift is the exception: crossterm reports uppercase G with it set, so requiring a
        // literally empty modifier set would take `G` and `N` away.
        dashboard.key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT));
        assert_eq!(
            dashboard.copy_status().as_deref(),
            Some(format!("COPY MOVE {},1", PANE_ROWS).as_str())
        );
    }

    #[test]
    fn copy_mode_publishes_a_status_line_for_the_footer_to_render() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event("run_1", b"hello world"));
        assert_eq!(
            dashboard.copy_status(),
            None,
            "there is no indicator when the mode is off"
        );
        dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        dashboard.key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
        let status = dashboard.copy_status().unwrap_or_default();
        assert!(
            status.contains("COPY"),
            "a modal mode must have something to show, got {status:?}"
        );
        dashboard.key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
        assert!(
            dashboard
                .copy_status()
                .unwrap_or_default()
                .contains("SELECT"),
            "starting a selection must be visible too"
        );
    }

    #[test]
    fn yanking_a_selection_reports_which_clipboard_route_was_used() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event("run_1", b"copy me\r\n"));
        dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        dashboard.key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
        dashboard.key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
        dashboard.key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
        dashboard.key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(!dashboard.copy_mode(), "yanking leaves copy mode");
        let notice = dashboard.error.clone().unwrap_or_default();
        assert!(
            notice.contains("copied") || notice.contains("clipboard"),
            "the yank must say what happened, got {notice:?}"
        );
    }

    // CONTROLLER RULING C2 again: the brief's version of both tests below used
    // `dashboard()`, whose pane "a" has `run_id: None`. Copy mode is refused on an unbound
    // pane, so neither test could have exercised anything. `bound_dashboard()` binds pane
    // "a" to "run_1", which is also the run the attach event seeds.
    #[test]
    fn copy_mode_is_visibly_signalled_in_the_pane_and_footer() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event("run_1", b"visible text\r\n"));
        let before = render_terminal(&mut dashboard, 100, 30);
        let area = *dashboard.pane_areas.get("a").expect("pane a is rendered");
        assert!(
            !row_text(&before, area, area.y).contains("COPY"),
            "the title must not claim copy mode before it is entered"
        );

        dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        dashboard.key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
        let terminal = render_terminal(&mut dashboard, 100, 30);

        // REVIEW FINDING 2: asserting `COPY` against the whole frame was passed entirely by
        // the footer's own indicator, so the title prefix had no coverage at all. The pane
        // title lives on the pane's top border row, so scope the search to exactly that.
        let title = row_text(&terminal, area, area.y);
        assert!(
            title.contains("COPY"),
            "the pane title must say it is in copy mode, got {title:?}"
        );

        let rendered = rendered(&terminal);
        // The brief asked for `contains('y')`, which every ordinary word in the UI already
        // satisfies; only a distinctive slice of the copy footer proves it actually changed.
        assert!(
            rendered.contains("y yank"),
            "the footer must publish the yank binding, got {rendered:?}"
        );
        assert!(
            !rendered.contains("keys go to the focused pane"),
            "the live-pane hint is wrong while every key is being swallowed"
        );
    }

    // REVIEW FINDING 1: `render_to_string` keeps only `cell.symbol()`, so deleting the whole
    // selection overlay left every test green. These two read the buffer's styles instead,
    // which is the only way this task's headline deliverable is guarded at all.
    #[test]
    fn the_selection_and_copy_cursor_are_painted_only_while_copy_mode_is_active() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event("run_1", b"hello world\r\n"));
        let selection = dashboard.theme.selection;
        let accent = dashboard.theme.accent;
        let surface = dashboard.theme.surface;

        let quiet = render_terminal(&mut dashboard, 100, 30);
        let area = *dashboard.pane_areas.get("a").expect("pane a is rendered");
        assert!(
            cells_with_background(&quiet, area, selection).is_empty(),
            "a pane that is not in copy mode must carry no selection highlight"
        );

        dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        dashboard.key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
        dashboard.key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
        dashboard.key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
        for _ in 0..3 {
            dashboard.key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
        }
        assert_eq!(
            dashboard.copy.as_ref().and_then(CopySession::selection),
            Some(((0, 0), (0, 3)))
        );

        let terminal = render_terminal(&mut dashboard, 100, 30);
        // The cursor cell is the fourth of the run and is painted as the cursor rather than
        // as selection, so the highlight is exactly the three cells behind it — and nothing
        // anywhere else in the pane.
        assert_eq!(
            cells_with_background(&terminal, area, selection),
            vec![(0, 0), (0, 1), (0, 2)],
            "the highlight must cover the selection and only the selection"
        );
        let buffer = terminal.backend().buffer();
        let cursor = &buffer[(area.x + 1 + 3, area.y + 1)];
        assert_eq!(cursor.bg, accent, "the copy cursor must be findable");
        assert_eq!(cursor.fg, surface, "and legible against its own block");
        assert_ne!(
            buffer[(area.x + 1 + 4, area.y + 1)].bg,
            selection,
            "the cell past the cursor is outside the selection"
        );

        dashboard.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let after = render_terminal(&mut dashboard, 100, 30);
        assert!(
            cells_with_background(&after, area, selection).is_empty(),
            "leaving copy mode must take the highlight with it"
        );
    }

    #[test]
    fn the_ptys_own_cursor_yields_to_the_copy_cursor_and_returns_afterwards() {
        let mut dashboard = bound_dashboard();
        // Ends on a newline, so the PTY cursor sits on a blank cell and tui-term draws its
        // block symbol there — which is what `draws_cursor` looks for.
        dashboard.apply_event(attach_event("run_1", b"hello world\r\n"));
        let live = render_terminal(&mut dashboard, 100, 30);
        let area = *dashboard.pane_areas.get("a").expect("pane a is rendered");
        assert!(
            draws_cursor(&live, area),
            "the focused live pane shows where typing lands"
        );

        dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        dashboard.key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
        let copying = render_terminal(&mut dashboard, 100, 30);
        assert!(
            !draws_cursor(&copying, area),
            "two cursor blocks make it ambiguous which one the keys are moving"
        );

        dashboard.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let back = render_terminal(&mut dashboard, 100, 30);
        assert!(
            draws_cursor(&back, area),
            "leaving copy mode gives the pane its own cursor back"
        );
    }

    #[test]
    fn dragging_across_a_pane_selects_without_writing_to_the_clipboard() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event("run_1", b"drag over me\r\n"));
        render_to_string(&mut dashboard, 100, 30);
        let area = *dashboard.pane_areas.get("a").expect("pane a is rendered");
        dashboard.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x + 2,
            row: area.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        assert!(
            !dashboard.copy_mode(),
            "a bare click focuses a pane; it must not trap the keyboard in a mode"
        );
        dashboard.mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: area.x + 8,
            row: area.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        assert!(
            dashboard.copy_mode(),
            "a drag inside a pane enters copy mode"
        );
        assert_eq!(
            dashboard
                .copy
                .as_ref()
                .and_then(CopySession::selection)
                .expect("the drag anchored a selection"),
            ((0, 1), (0, 7)),
            "the anchor is the press cell and the cursor follows the pointer"
        );
        dashboard.mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: area.x + 8,
            row: area.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        assert!(
            dashboard.copy_mode(),
            "releasing finalises the selection but stays in copy mode"
        );
        assert!(
            dashboard.error.is_none(),
            "releasing a drag must not write to the clipboard"
        );
        let rendered = render_to_string(&mut dashboard, 100, 30);
        assert!(
            rendered.contains("COPY"),
            "a pointer selection puts the pane in the same visible mode a keyboard one does"
        );
    }

    #[test]
    fn copy_mode_is_refused_on_a_pane_with_no_run() {
        let mut dashboard = dashboard();
        dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        dashboard.key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
        assert!(!dashboard.copy_mode());
        assert!(
            dashboard.error.is_some(),
            "an impossible command must explain itself rather than doing nothing"
        );
    }
}
