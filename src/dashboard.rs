use std::collections::HashMap;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};

use crate::{
    adapter::{AdapterId, AdapterSelection},
    detect::{AgentKind, AgentState},
    discovery::ExternalAgentCandidate,
    layout::{LayoutNode, LayoutSnapshot, PaneLayout, PaneRuntime, SplitAxis, WorkspaceLayout},
    protocol::{
        BindingKind, DashboardProfile, DispatchRequest, Event, LaunchIntoPaneRequest,
        PaneInputRequest, Request, RuntimeSnapshot, TerminalLaunchRequest, WorkspaceRequest,
    },
    terminal::PaneScreen,
};

const MIN_PANE_WIDTH: u16 = 8;
const MIN_PANE_HEIGHT: u16 = 3;

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
    pub input_mode: bool,
    /// This client's own emulator for each run, advanced by pushed deltas. The daemon holds the
    /// authoritative screen; this is the local replica the dashboard actually paints from.
    pub screens: HashMap<String, PaneScreen>,
    /// Latest agent identity and state per run, as pushed by the daemon.
    pub agents: HashMap<String, (Option<AgentKind>, AgentState)>,
    revisions: HashMap<String, u64>,
    needs_refresh: bool,
    pending_resizes: Vec<(String, String, u16, u16)>,
    pane_areas: HashMap<String, Rect>,
    dividers: Vec<Divider>,
    dragging: Option<DragTarget>,
    sequence: u64,
    dismiss_external_area: Option<Rect>,
    launch_area: Option<Rect>,
    launch_form: Option<LaunchForm>,
    launch_profile_areas: Vec<Rect>,
    launch_confirm_area: Option<Rect>,
    launch_mode_area: Option<Rect>,
    help_open: bool,
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
                let mut terminal = PaneScreen::new(rows, cols, 0);
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
    }

    /// True once when a pushed event invalidated the run list or layout. The render loop uses
    /// this instead of an unconditional timer poll, so an idle dashboard issues no requests.
    pub fn take_refresh(&mut self) -> bool {
        std::mem::take(&mut self.needs_refresh)
    }

    /// Pane geometry changes the render pass discovered, as `(workspace_id, pane_id, rows, cols)`.
    /// Rendering is Task 12's, so nothing queues into this yet and the queue is always empty.
    pub fn take_pending_resizes(&mut self) -> Vec<(String, String, u16, u16)> {
        std::mem::take(&mut self.pending_resizes)
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
        self.dividers.clear();
        self.dismiss_external_area = None;
        self.launch_area = None;
        self.launch_profile_areas.clear();
        self.launch_confirm_area = None;
        self.launch_mode_area = None;
        let area = frame.area();
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
            self.render_node(frame, panes, &workspace, &workspace.root);
        } else {
            frame.render_widget(
                Paragraph::new("No workspace yet. Press n to create one.")
                    .block(Block::default().borders(Borders::ALL).title(" RUNTIME ")),
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
        let notice = self.error.as_deref().unwrap_or(if self.input_mode {
            "INPUT MODE · bytes go only to focused Dock-owned run · Esc exits (not forwarded)"
        } else if self.help_open {
            "HELP · Esc/? closes"
        } else if self.rename_form.is_some() {
            "RENAME · type a pane name · Enter saves · Esc cancels"
        } else {
            "[n] workspace [h/v] split [Tab/←↑→↓] focus [r] rename [x] close [l] launch [i] input [?] help [q] quit"
        });
        frame.render_widget(
            Paragraph::new(notice).style(Style::default().fg(if self.error.is_some() {
                Color::Red
            } else {
                Color::DarkGray
            })),
            footer,
        );
    }

    fn render_header(&self, frame: &mut Frame, area: Rect) {
        let workspace = self.workspace().map(|w| w.name.as_str()).unwrap_or("empty");
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    " d·ock ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!(" runtime · {workspace} · protocol v6")),
            ]))
            .block(Block::default().borders(Borders::BOTTOM)),
            area,
        );
    }

    fn render_sidebar(&mut self, frame: &mut Frame, area: Rect) {
        let mut lines = vec![Line::styled(
            "WORKSPACES",
            Style::default().add_modifier(Modifier::BOLD),
        )];
        for (index, workspace) in self.layout.workspaces.iter().enumerate() {
            lines.push(Line::styled(
                format!(
                    "{} {}",
                    if index == self.workspace_index {
                        "›"
                    } else {
                        " "
                    },
                    workspace.name
                ),
                Style::default().fg(if index == self.workspace_index {
                    Color::Cyan
                } else {
                    Color::Gray
                }),
            ));
        }
        lines.push(Line::from(""));
        lines.push(Line::styled(
            "EXISTING AGENTS",
            Style::default().add_modifier(Modifier::BOLD),
        ));
        if self.external.is_empty() {
            lines.push(Line::styled(
                " none discovered",
                Style::default().fg(Color::DarkGray),
            ));
        }
        for candidate in &self.external {
            lines.push(Line::from(candidate.provider.as_str()));
            lines.push(Line::styled(
                candidate.status(),
                Style::default().fg(Color::Yellow),
            ));
        }
        if !self.external.is_empty() {
            lines.push(Line::styled(
                " [d] dismiss all",
                Style::default().fg(Color::Cyan),
            ));
            let row = area.y + u16::try_from(lines.len()).unwrap_or(u16::MAX) - 1;
            self.dismiss_external_area = Some(Rect::new(area.x, row, area.width, 1));
        }
        lines.push(Line::from(""));
        lines.push(Line::styled(
            "[l] LAUNCH DOCK AGENT",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ));
        let row = area.y + u16::try_from(lines.len()).unwrap_or(u16::MAX) - 1;
        self.launch_area = Some(Rect::new(area.x, row, area.width, 1));
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: true })
                .block(Block::default().borders(Borders::RIGHT)),
            area,
        );
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
                let run = pane
                    .run_id
                    .as_deref()
                    .and_then(|id| self.runs.iter().find(|run| run.run_id == id));
                let output = match run {
                    Some(run) => format!(
                        // The snapshot no longer carries pane text; emulated screens reach the
                        // dashboard over pane subscriptions instead of being re-sent by polling.
                        "mode: {}\nrepository: {}\ntask: {}\nrun: {}\nbinding: {}/{}\nsize: {}x{}",
                        if run.binding_kind == BindingKind::Terminal { "unbound terminal" } else { "repository dispatch" },
                        if run.binding_kind == BindingKind::Terminal { "unbound" } else { &run.repository_root },
                        if run.binding_kind == BindingKind::Terminal { "unbound" } else { &run.external_task_ref },
                        run.run_id,
                        run.workspace_id,
                        run.pane_id,
                        run.cols,
                        run.rows
                    ),
                    None if pane.run_id.is_some() => format!(
                        "repository: unavailable\ntask: unavailable\nrun: {}\nbinding: {}/{}\n\nDock-owned run facts are unavailable.",
                        pane.run_id.as_deref().unwrap_or_default(),
                        workspace.workspace_id,
                        pane.pane_id
                    ),
                    None => "repository: unbound\ntask: unbound\nrun: unbound\nbinding: unbound\n\nNo Dock-owned run bound.".into(),
                };
                let title = format!(" {} · {} ", pane.name, runtime_label(pane.runtime));
                frame.render_widget(
                    Paragraph::new(output)
                        .wrap(Wrap { trim: false })
                        .style(Style::default().fg(runtime_color(pane.runtime)))
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .title(title)
                                .border_style(
                                    Style::default()
                                        .fg(if focused {
                                            Color::Cyan
                                        } else {
                                            Color::DarkGray
                                        })
                                        .add_modifier(if focused {
                                            Modifier::BOLD
                                        } else {
                                            Modifier::empty()
                                        }),
                                ),
                        ),
                    area,
                );
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

    fn render_narrow(&self, frame: &mut Frame, area: Rect) {
        let mut lines = vec![Line::styled(
            "d·ock · compact runtime",
            Style::default().fg(Color::Cyan),
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
                    Style::default().fg(runtime_color(pane.runtime)),
                ));
            }
        } else {
            lines.push(Line::from("No workspace · n create"));
        }
        lines.push(Line::styled(
            "n new · h/v split · Tab focus · l launch · ? help · q quit",
            Style::default().fg(Color::DarkGray),
        ));
        frame.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL)),
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
        let lines = vec![
            Line::styled("DAILY", Style::default().add_modifier(Modifier::BOLD)),
            Line::from("n new workspace   h/v split   Tab/arrows focus"),
            Line::from("r rename   x close   l launch   i input   q quit   ? help"),
            Line::styled("LAYOUT", Style::default().add_modifier(Modifier::BOLD)),
            Line::from("←/↑ previous focus   →/↓/Tab next focus   +/- resize focused split"),
            Line::styled("FORMS", Style::default().add_modifier(Modifier::BOLD)),
            Line::from("type to filter/edit   ↑/↓ or j/k select   Enter review/confirm"),
            Line::from("Esc always cancels a form or exits input mode; it is never forwarded"),
            Line::styled("CURRENT", Style::default().add_modifier(Modifier::BOLD)),
            Line::from(if self.workspace().is_some() {
                "Workspace selected; pane commands are available."
            } else {
                "No workspace: create one with n before pane actions."
            }),
            Line::from("Esc or ? closes help"),
        ];
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: true })
                .block(Block::default().borders(Borders::ALL).title(" KEYMAP ")),
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
            .block(
                Block::default()
                    .borders(Borders::ALL)
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
        if self.input_mode {
            if key.code == KeyCode::Esc {
                self.input_mode = false;
                return UiCommand::None;
            }
            let input = match key.code {
                KeyCode::Char(character)
                    if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
                {
                    character.to_string()
                }
                KeyCode::Enter => "\n".into(),
                KeyCode::Backspace => "\u{7f}".into(),
                _ => return UiCommand::None,
            };
            let Some(workspace) = self.workspace() else {
                return UiCommand::None;
            };
            return UiCommand::Request(Box::new(Request::PaneInput(PaneInputRequest {
                workspace_id: workspace.workspace_id.clone(),
                pane_id: workspace.focused_pane_id.clone(),
                // Protocol v7 carries pane input base64-encoded; raw text is rejected by the
                // daemon's decode and would corrupt any control byte that did get through.
                input: PaneInputRequest::encode(input.as_bytes()),
            })));
        }
        match key.code {
            KeyCode::Char('?') => {
                self.error = None;
                self.help_open = true;
                UiCommand::None
            }
            KeyCode::Char('q') => UiCommand::Quit,
            KeyCode::Char('i') => {
                if self.focused_owned_run().is_some() {
                    self.input_mode = true;
                    self.error = None;
                } else {
                    self.error =
                        Some("input unavailable: focused pane has no Dock-owned run".into());
                }
                UiCommand::None
            }
            KeyCode::Char('n') => {
                let workspace_id = self.next_unique_id("workspace");
                let pane_id = self.next_unique_id("pane");
                UiCommand::Request(Box::new(Request::Workspace(WorkspaceRequest::Create {
                    name: workspace_id.replace('_', " "),
                    workspace_id,
                    pane_id,
                })))
            }
            KeyCode::Char('d') => {
                self.external.clear();
                UiCommand::None
            }
            KeyCode::Char('l') => {
                self.open_launch();
                UiCommand::LoadCatalog
            }
            KeyCode::Char('[') => {
                self.workspace_index = self.workspace_index.saturating_sub(1);
                UiCommand::None
            }
            KeyCode::Char(']') => {
                if self.workspace_index + 1 < self.layout.workspaces.len() {
                    self.workspace_index += 1;
                }
                UiCommand::None
            }
            KeyCode::Tab
            | KeyCode::BackTab
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Up
            | KeyCode::Down => self.focus_next(matches!(
                key.code,
                KeyCode::BackTab | KeyCode::Left | KeyCode::Up
            )),
            KeyCode::Char('h') => self.split(SplitAxis::Horizontal),
            KeyCode::Char('v') => self.split(SplitAxis::Vertical),
            KeyCode::Char('r') => self.rename(),
            KeyCode::Char('x') => self.close(),
            KeyCode::Char('+') | KeyCode::Char('=') => self.resize_keyboard(50),
            KeyCode::Char('-') => self.resize_keyboard(-50),
            _ => UiCommand::None,
        }
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
                    Color::Black
                } else if available {
                    Color::Green
                } else {
                    Color::DarkGray
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
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
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

    fn focused_owned_run(&self) -> Option<&RuntimeSnapshot> {
        let workspace = self.workspace()?;
        let run_id = workspace
            .panes
            .get(&workspace.focused_pane_id)?
            .run_id
            .as_deref()?;
        self.runs.iter().find(|run| {
            run.run_id == run_id
                && run.workspace_id == workspace.workspace_id
                && run.pane_id == workspace.focused_pane_id
        })
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
                self.layout.workspaces[self.workspace_index].focused_pane_id = pane_id.clone();
                UiCommand::Request(Box::new(Request::Workspace(WorkspaceRequest::Focus {
                    workspace_id,
                    pane_id,
                })))
            }
            MouseEventKind::Drag(MouseButton::Left) => {
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
pub fn runtime_color(runtime: PaneRuntime) -> Color {
    match runtime {
        PaneRuntime::Running => Color::Green,
        PaneRuntime::Exited => Color::Red,
        PaneRuntime::Restored => Color::Yellow,
        PaneRuntime::Empty => Color::DarkGray,
    }
}
fn runtime_label(runtime: PaneRuntime) -> &'static str {
    match runtime {
        PaneRuntime::Running => "running",
        PaneRuntime::Exited => "exited",
        PaneRuntime::Restored => "restored",
        PaneRuntime::Empty => "empty",
    }
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
    fn renders_split_focus_states_and_narrow_fallback() {
        for (width, height) in [(90, 24), (40, 10)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut dashboard = dashboard();
            terminal.draw(|frame| dashboard.render(frame)).unwrap();
            let text = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(text.contains("Daily"));
            assert!(text.contains(if width < 52 { "compact" } else { "editor" }));
        }
        assert_eq!(runtime_color(PaneRuntime::Running), Color::Green);
        assert_eq!(runtime_color(PaneRuntime::Restored), Color::Yellow);
    }

    #[test]
    fn keyboard_and_mouse_focus_and_bounded_resize() {
        let mut dashboard = dashboard();
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal.draw(|frame| dashboard.render(frame)).unwrap();
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
        assert!(!dashboard.input_mode);
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
        let create = dashboard.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(matches!(
            create,
            UiCommand::Request(request)
                if matches!(request.as_ref(), Request::Workspace(WorkspaceRequest::Create { workspace_id, pane_id, .. })
                    if workspace_id == "workspace_4" && pane_id == "pane_5")
        ));
        let split = dashboard.key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
        assert!(matches!(
            split,
            UiCommand::Request(request)
                if matches!(request.as_ref(), Request::Workspace(WorkspaceRequest::Split { new_pane_id, .. })
                    if new_pane_id == "pane_6")
        ));
    }

    #[test]
    fn renders_runtime_binding_facts_and_explicit_unbound_facts() {
        let mut dashboard = dashboard();
        dashboard.layout.workspaces[0]
            .panes
            .get_mut("a")
            .unwrap()
            .run_id = Some("dock_real".into());
        dashboard.runs.push(snapshot());
        let mut terminal = Terminal::new(TestBackend::new(110, 28)).unwrap();
        terminal.draw(|frame| dashboard.render(frame)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("repository: /repo/real"));
        assert!(text.contains("task: TASK-61"));
        assert!(text.contains("binding: w/a"));
        assert!(text.contains("repository: unbound"));
        assert!(text.contains("task: unbound"));
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
        assert_eq!(
            dashboard.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)),
            UiCommand::None
        );
        assert!(dashboard.external.is_empty());
        assert_eq!(
            dashboard.key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)),
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
        assert_eq!(
            dashboard.key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE)),
            UiCommand::None
        );
        assert!(dashboard.help_open);
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal.draw(|frame| dashboard.render(frame)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        for key in [
            "n new workspace",
            "h/v split",
            "Tab/arrows focus",
            "r rename",
            "x close",
            "l launch",
            "i input",
            "q quit",
            "? help",
        ] {
            assert!(text.contains(key), "missing published mnemonic: {key}");
        }
        assert_eq!(
            dashboard.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            UiCommand::None
        );
        assert!(!dashboard.help_open);
    }

    #[test]
    fn input_mode_requires_owned_binding_and_escape_never_forwards_a_byte() {
        let mut dashboard = dashboard();
        assert_eq!(
            dashboard.key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)),
            UiCommand::None
        );
        assert_eq!(
            dashboard.error.as_deref(),
            Some("input unavailable: focused pane has no Dock-owned run")
        );
        dashboard.layout.workspaces[0]
            .panes
            .get_mut("a")
            .unwrap()
            .run_id = Some("dock_real".into());
        dashboard.runs.push(snapshot());
        assert_eq!(
            dashboard.key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)),
            UiCommand::None
        );
        assert!(dashboard.input_mode);
        assert_eq!(
            dashboard.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            UiCommand::None
        );
        assert!(!dashboard.input_mode);
        assert!(matches!(
            dashboard.key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
            UiCommand::Quit
        ));
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
        let command = dashboard.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(matches!(command, UiCommand::Request(_)));
        assert_eq!(dashboard.workspace().unwrap().focused_pane_id, "b");
        let panes = dashboard.workspace().unwrap().panes.len();
        let command = dashboard.key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
        assert!(matches!(command, UiCommand::Request(_)));
        assert_eq!(dashboard.workspace().unwrap().panes.len(), panes + 1);
        assert_eq!(
            dashboard.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)),
            UiCommand::None
        );
        assert!(dashboard.rename_form.is_some());
        assert_eq!(
            dashboard.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            UiCommand::None
        );
        assert!(dashboard.rename_form.is_none());
        assert_eq!(
            dashboard.key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)),
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
        for key in ['i', 'h', 'r', 'x', '+'] {
            assert_eq!(
                dashboard.key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE)),
                UiCommand::None
            );
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
}
