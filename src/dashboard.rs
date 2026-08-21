use std::collections::HashMap;

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
    discovery::ExternalAgentCandidate,
    layout::{LayoutNode, LayoutSnapshot, PaneRuntime, SplitAxis, WorkspaceLayout},
    protocol::{
        DispatchRequest, LaunchIntoPaneRequest, PaneInputRequest, Request, RuntimeSnapshot,
        WorkspaceRequest,
    },
};

const MIN_PANE_WIDTH: u16 = 8;
const MIN_PANE_HEIGHT: u16 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiCommand {
    Request(Box<Request>),
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
    pub workspace_index: usize,
    pub error: Option<String>,
    pub input_mode: bool,
    pane_areas: HashMap<String, Rect>,
    dividers: Vec<Divider>,
    dragging: Option<DragTarget>,
    sequence: u64,
    dismiss_external_area: Option<Rect>,
    launch_area: Option<Rect>,
}

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
    pub fn workspace(&self) -> Option<&WorkspaceLayout> {
        self.layout.workspaces.get(self.workspace_index)
    }

    pub fn render(&mut self, frame: &mut Frame) {
        self.pane_areas.clear();
        self.dividers.clear();
        self.dismiss_external_area = None;
        self.launch_area = None;
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
        let notice = self.error.as_deref().unwrap_or(if self.input_mode {
            "INPUT → selected Dock-owned pane · Esc exits input mode"
        } else {
            "[n] workspace  [l] launch owned fixture  [i] input  [d] dismiss externals  [Tab] focus  [q] quit"
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
            "[l] LAUNCH DOCK FIXTURE",
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
                        "repository: {}\ntask: {}\nrun: {}\nbinding: {}/{}\n\n{}",
                        run.repository_root,
                        run.external_task_ref,
                        run.run_id,
                        run.workspace_id,
                        run.pane_id,
                        run.scrollback
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
            for pane in workspace.panes.values() {
                lines.push(Line::styled(
                    format!(
                        "{} {} · {}",
                        if pane.pane_id == workspace.focused_pane_id {
                            "›"
                        } else {
                            " "
                        },
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
            "q quit · Tab focus · h/v split",
            Style::default().fg(Color::DarkGray),
        ));
        frame.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL)),
            area,
        );
    }

    pub fn key(&mut self, key: KeyEvent) -> UiCommand {
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
                input,
            })));
        }
        match key.code {
            KeyCode::Char('q') => UiCommand::Quit,
            KeyCode::Char('i') => {
                self.input_mode = true;
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
            KeyCode::Char('l') => self.launch_fixture(),
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
            _ => UiCommand::None,
        }
    }

    fn launch_fixture(&mut self) -> UiCommand {
        if self.repository_root.is_empty() {
            self.error = Some("cannot launch: repository is unbound".into());
            return UiCommand::None;
        }
        let Some(workspace) = self.workspace() else {
            self.error = Some("cannot launch: create a workspace first".into());
            return UiCommand::None;
        };
        let workspace_id = workspace.workspace_id.clone();
        let pane_id = workspace.focused_pane_id.clone();
        let run_id = self.next_unique_id("dock_ui");
        UiCommand::Request(Box::new(Request::LaunchIntoPane(LaunchIntoPaneRequest {
            workspace_id,
            pane_id,
            dispatch: DispatchRequest {
                repository_root: self.repository_root.clone(),
                external_task_ref: format!("ui-{run_id}"),
                run_id,
                worktree: self.repository_root.clone(),
                adapter: AdapterSelection {
                    id: AdapterId::Fixture,
                    executable: None,
                    arguments: vec![
                        "-c".into(),
                        "printf 'Dock-owned fixture ready\\n'; sleep 30".into(),
                    ],
                },
            },
        })))
    }

    fn focus_next(&self, reverse: bool) -> UiCommand {
        let Some(workspace) = self.workspace() else {
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
        UiCommand::Request(Box::new(Request::Workspace(WorkspaceRequest::Focus {
            workspace_id: workspace.workspace_id.clone(),
            pane_id: ids[next].to_string(),
        })))
    }

    fn split(&mut self, axis: SplitAxis) -> UiCommand {
        let Some((workspace_id, pane_id)) = self.workspace().map(|workspace| {
            (
                workspace.workspace_id.clone(),
                workspace.focused_pane_id.clone(),
            )
        }) else {
            return UiCommand::None;
        };
        let new_pane_id = self.next_unique_id("pane");
        UiCommand::Request(Box::new(Request::Workspace(WorkspaceRequest::Split {
            workspace_id,
            pane_id,
            new_pane_id,
            axis,
        })))
    }

    fn rename(&mut self) -> UiCommand {
        let Some((workspace_id, pane_id)) = self.workspace().map(|workspace| {
            (
                workspace.workspace_id.clone(),
                workspace.focused_pane_id.clone(),
            )
        }) else {
            return UiCommand::None;
        };
        self.sequence += 1;
        UiCommand::Request(Box::new(Request::Workspace(WorkspaceRequest::Rename {
            workspace_id,
            pane_id: Some(pane_id),
            name: format!("pane {}", self.sequence),
        })))
    }

    fn close(&self) -> UiCommand {
        let Some(workspace) = self.workspace() else {
            return UiCommand::None;
        };
        UiCommand::Request(Box::new(Request::Workspace(WorkspaceRequest::Close {
            workspace_id: workspace.workspace_id.clone(),
            pane_id: workspace.focused_pane_id.clone(),
        })))
    }

    pub fn mouse(&mut self, event: MouseEvent) -> UiCommand {
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
                    return self.launch_fixture();
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
                let Some(workspace) = self.workspace() else {
                    return UiCommand::None;
                };
                UiCommand::Request(Box::new(Request::Workspace(WorkspaceRequest::Resize {
                    workspace_id: workspace.workspace_id.clone(),
                    pane_id: divider.pane_id.clone(),
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
            scrollback: "owned output".into(),
            scrollback_bytes: 12,
            scrollback_capacity_bytes: 1024,
            scrollback_truncated: false,
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
        dashboard.external.push(ExternalAgentCandidate {
            provider: "Codex CLI".into(),
            repository_match: false,
        });
        assert_eq!(
            dashboard.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)),
            UiCommand::None
        );
        assert!(dashboard.external.is_empty());
        assert!(
            matches!(dashboard.key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)), UiCommand::Request(request)
            if matches!(request.as_ref(), Request::LaunchIntoPane(request) if request.dispatch.adapter.id == AdapterId::Fixture && request.dispatch.repository_root == "/repo" && request.workspace_id == "w" && request.pane_id == "a"))
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
        assert!(
            matches!(dashboard.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: launch.x + 1, row: launch.y, modifiers: KeyModifiers::NONE }), UiCommand::Request(request)
            if matches!(request.as_ref(), Request::LaunchIntoPane(request) if request.dispatch.adapter.id == AdapterId::Fixture && request.workspace_id == "w" && request.pane_id == "a"))
        );
    }
}
