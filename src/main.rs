mod app;
mod git;
mod kanban;
mod model;

use std::{error::Error, io};

use app::{Action, App};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use model::{BoardFixture, Task, TaskState};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
};

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(worktree) = args
        .iter()
        .find_map(|argument| argument.strip_prefix("--git-dir=").map(str::to_owned))
    {
        let base = args
            .iter()
            .find_map(|argument| argument.strip_prefix("--base=").map(str::to_owned))
            .unwrap_or_else(|| "HEAD".into());
        let adapter = git::GitAdapter::new(worktree);
        let facts = adapter.facts(&base).map_err(io::Error::other)?;
        let (diff, rendered_with_delta) = adapter.render_diff(&base).map_err(io::Error::other)?;
        println!(
            "worktree={}\nbranch={}\nbase={}\nhead={}\nfiles={} +{} -{}\ndelta={}\n\n{}",
            facts.worktree.display(),
            facts.branch,
            facts.base_sha,
            facts.head_sha,
            facts.changed_files,
            facts.insertions,
            facts.deletions,
            rendered_with_delta,
            diff
        );
        return Ok(());
    }
    if let Some(board_dir) = args
        .iter()
        .find_map(|argument| argument.strip_prefix("--kanban-dir=").map(str::to_owned))
    {
        let adapter = kanban::KanbanMdAdapter::new(board_dir);
        if let Some(claim) = args
            .iter()
            .find_map(|argument| argument.strip_prefix("--claim=").map(str::to_owned))
        {
            let task = adapter
                .pick(&claim, "backlog", "in-progress")
                .map_err(io::Error::other)?;
            println!("claimed {}\t{}\t{}", task.id, task.status, task.title);
            return Ok(());
        }
        let tasks = adapter.list().map_err(io::Error::other)?;
        for task in tasks {
            println!(
                "{}\t{}\t{}\t{}",
                task.id,
                task.status,
                task.claimed_by.unwrap_or_else(|| "unclaimed".into()),
                task.title
            );
        }
        return Ok(());
    }
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let result = run(&mut terminal, App::new(BoardFixture::example()));
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(result?)
}

fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, mut app: App) -> io::Result<()> {
    while !app.should_quit {
        terminal.draw(|frame| render(frame, &app))?;
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Char('q') => app.should_quit = true,
                KeyCode::Char('j') | KeyCode::Down => app.apply(Action::MoveDown),
                KeyCode::Char('k') | KeyCode::Up => app.apply(Action::MoveUp),
                KeyCode::Char('a') => app.apply(Action::AcceptScope),
                KeyCode::Char('r') => app.apply(Action::RequestChanges),
                KeyCode::Char('l') => app.apply(Action::OpenLazygit),
                _ => {}
            }
        }
    }
    Ok(())
}

fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(12),
            Constraint::Length(3),
        ])
        .split(area);
    render_header(frame, rows[0], app);
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(rows[1]);
    render_board(frame, columns[0], app);
    render_handoff(frame, columns[1], app.selected_task());
    frame.render_widget(
        Paragraph::new(app.notice.as_str())
            .style(Style::default().fg(Color::Cyan))
            .block(Block::default().borders(Borders::TOP).title(" DOCK EVENT ")),
        rows[2],
    );
}

fn render_header(frame: &mut Frame, area: Rect, app: &App) {
    let title = Line::from(vec![
        Span::styled(
            "  d·ock ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            " {}  ·  {}",
            app.board.project, app.board.herdr_status
        )),
        Span::styled(
            "     [j/k] select  [a] accept scope  [r] request changes  [l] LazyGit  [q] quit",
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(title).block(Block::default().borders(Borders::BOTTOM)),
        area,
    );
}

fn render_board(frame: &mut Frame, area: Rect, app: &App) {
    let items: Vec<ListItem> = app
        .board
        .tasks
        .iter()
        .enumerate()
        .map(|(index, task)| {
            let marker = if index == app.selected { "›" } else { " " };
            let color = state_color(task.state);
            ListItem::new(vec![
                Line::from(Span::styled(
                    format!("{} {}  {}", marker, task.id, task.state.label()),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::raw(format!("    {}", task.title))),
                Line::from(Span::styled(
                    format!("    {} · {}", task.agent, task.branch),
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(""),
            ])
        })
        .collect();
    frame.render_widget(
        List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" AGENT HANDOFFS "),
        ),
        area,
    );
}

fn render_handoff(frame: &mut Frame, area: Rect, task: &Task) {
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Length(7),
            Constraint::Length(5),
            Constraint::Min(8),
        ])
        .split(area);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                format!("{} · {}", task.id, task.title),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(format!(
                "{}  |  {}  |  base {}",
                task.worktree, task.branch, task.base_sha
            )),
        ])
        .block(Block::default().borders(Borders::ALL).title(" BOUND RUN ")),
        sections[0],
    );

    let mut handoff = vec![Line::from(task.handoff_summary.as_str())];
    if let Some(question) = &task.question {
        handoff.push(Line::from(""));
        handoff.push(Line::from(Span::styled(
            format!("DECISION NEEDED: {}", question),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));
    }
    frame.render_widget(
        Paragraph::new(handoff).wrap(Wrap { trim: true }).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" AGENT HANDOFF "),
        ),
        sections[1],
    );

    let checks = if task.checks.is_empty() {
        "No declared checks recorded yet.".to_owned()
    } else {
        task.checks
            .iter()
            .map(|c| format!("{} {}", if c.passed { "✓" } else { "!" }, c.name))
            .collect::<Vec<_>>()
            .join("     ")
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(format!(
                "{} files changed  +{} / -{}",
                task.changed_files, task.insertions, task.deletions
            )),
            Line::from(checks),
        ])
        .block(Block::default().borders(Borders::ALL).title(" EVIDENCE ")),
        sections[2],
    );

    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                "Delta-framed diff preview",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "+ pub fn redact_header(value: &str) -> String {",
                Style::default().fg(Color::Green),
            )),
            Line::from(Span::styled(
                "+     REDACTED.replace_all(value)",
                Style::default().fg(Color::Green),
            )),
            Line::from(Span::styled(
                "- persist(receipt.headers)",
                Style::default().fg(Color::Red),
            )),
            Line::from(Span::styled(
                "+ persist(redact_receipt(receipt))",
                Style::default().fg(Color::Green),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "[a] accept scope     [r] route changes     [l] open this worktree in LazyGit",
                Style::default().fg(Color::DarkGray),
            )),
        ])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" DIFF · delta adapter next "),
        ),
        sections[3],
    );
}

fn state_color(state: TaskState) -> Color {
    match state {
        TaskState::NeedsInput => Color::Yellow,
        TaskState::NeedsReview => Color::Cyan,
        TaskState::ChangesRequested => Color::Red,
        TaskState::ReadyToMerge | TaskState::Done => Color::Green,
        TaskState::Running => Color::Blue,
        TaskState::Todo => Color::DarkGray,
    }
}
