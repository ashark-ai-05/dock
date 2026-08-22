//! Reading the kanban board straight from its task files.
//!
//! `kanban-md` owns the board and is what moves a task between statuses, but reading does not need
//! it: the tasks are Markdown files with YAML front matter, and they are in the repository the user
//! already has open. Parsing them directly means the board renders on a machine where the binary is
//! not installed, which is most machines — and a board you can read but not yet claim from is worth
//! considerably more than an error message.

use std::{
    fs,
    path::{Path, PathBuf},
};

/// One task, as its file declares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardTask {
    pub id: u64,
    pub title: String,
    pub status: String,
    pub priority: String,
    pub file: PathBuf,
}

/// The order statuses are shown in, which is the order work moves through them.
///
/// A status the board uses but this list does not know is not dropped; it sorts after these, so an
/// unfamiliar column is still visible rather than silently missing.
const STATUS_ORDER: [&str; 5] = ["in-progress", "review", "backlog", "todo", "done"];

/// Where the board a dashboard should show actually lives.
///
/// A repository's own board comes first: tasks belonging to a project belong beside it, and that is
/// where `kanban-md` already looks. But work exists that no repository owns — the user's own, and
/// whatever their agents are doing for them — and a dashboard opened outside a repository had
/// nothing to show at all. So it falls back to a personal board under the home directory, which
/// survives moving between projects and belongs to no project in particular.
///
/// Returns `None` only when neither a repository nor a home directory can be determined, which
/// leaves nowhere a board could sensibly live.
pub fn tasks_dir(repository_root: &str) -> Option<PathBuf> {
    let repository = repository_root.trim();
    if !repository.is_empty() {
        return Some(Path::new(repository).join("kanban").join("tasks"));
    }
    personal_tasks_dir()
}

/// The personal board: `~/.dock/board/tasks`.
pub fn personal_tasks_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    if home.is_empty() {
        return None;
    }
    Some(
        PathBuf::from(home)
            .join(".dock")
            .join("board")
            .join("tasks"),
    )
}

/// Whether this directory is the personal board rather than a repository's.
///
/// Dock writes tasks only to its own board. A repository's board is owned by `kanban-md` and by
/// whoever else commits to it, and creating files in someone else's board is not Dock's to do.
pub fn is_personal(directory: &Path) -> bool {
    personal_tasks_dir().is_some_and(|personal| personal == directory)
}

/// Writes a new task onto a board and returns it.
///
/// Only ever called for Dock's own personal board. A repository's board belongs to `kanban-md` and
/// to whoever else commits to it, and creating files in someone else's board is not Dock's to do.
///
/// The id is one past the highest already present, so it stays stable against a board that other
/// tools also write to: reusing a free gap would collide with a task that was archived rather than
/// deleted.
pub fn create(directory: &Path, title: &str) -> Result<BoardTask, String> {
    let title = title.trim();
    if title.is_empty() {
        return Err("a task needs a title".into());
    }
    fs::create_dir_all(directory)
        .map_err(|error| format!("could not create the board directory: {error}"))?;
    let id = load(directory)
        .iter()
        .map(|task| task.id)
        .max()
        .unwrap_or(0)
        + 1;
    let slug: String = title
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .take(6)
        .collect::<Vec<_>>()
        .join("-");
    let file = directory.join(format!("{id:03}-{slug}.md"));
    // Single quotes around the title, matching the front matter these files already use, with any
    // quote of its own removed rather than escaped: a title is not worth a YAML quoting dialect.
    let safe_title = title.replace('\'', "");
    let body = format!(
        "---\nid: {id}\ntitle: '{safe_title}'\nstatus: backlog\npriority: medium\nclass: standard\n---\n\n# Outcome\n\n{safe_title}\n"
    );
    fs::write(&file, body).map_err(|error| format!("could not write the task: {error}"))?;
    Ok(BoardTask {
        id,
        title: safe_title,
        status: "backlog".into(),
        priority: "medium".into(),
        file,
    })
}

/// Every task in `directory`, ordered by status then id.
///
/// Best-effort by design: a file that cannot be read or has no `id` is skipped rather than failing
/// the board, because one malformed task should not hide the other eleven.
pub fn load(directory: &Path) -> Vec<BoardTask> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut tasks: Vec<BoardTask> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "md"))
        .filter_map(|path| {
            let text = fs::read_to_string(&path).ok()?;
            parse(&text, &path)
        })
        .collect();
    tasks.sort_by(|a, b| {
        status_rank(&a.status)
            .cmp(&status_rank(&b.status))
            .then(a.status.cmp(&b.status))
            .then(a.id.cmp(&b.id))
    });
    tasks
}

fn status_rank(status: &str) -> usize {
    STATUS_ORDER
        .iter()
        .position(|known| *known == status)
        .unwrap_or(STATUS_ORDER.len())
}

/// Reads the scalar fields out of a task file's front matter.
///
/// Deliberately not a YAML parser. The four fields wanted are plain `key: value` scalars on their
/// own lines, and the rest of the front matter is lists (`tags`, `depends_on`) whose indented items
/// must not be mistaken for keys — so only unindented lines between the `---` fences are read, and
/// everything else is ignored rather than interpreted.
fn parse(text: &str, path: &Path) -> Option<BoardTask> {
    let mut lines = text.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    let (mut id, mut title, mut status, mut priority) = (None, None, None, None);
    for line in lines {
        if line.trim() == "---" {
            break;
        }
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = unquote(value.trim());
        match key.trim() {
            "id" => id = value.parse::<u64>().ok(),
            "title" => title = Some(value.to_owned()),
            "status" => status = Some(value.to_owned()),
            "priority" => priority = Some(value.to_owned()),
            _ => {}
        }
    }
    Some(BoardTask {
        id: id?,
        title: title.unwrap_or_default(),
        status: status.unwrap_or_else(|| "unknown".into()),
        priority: priority.unwrap_or_default(),
        file: path.to_path_buf(),
    })
}

fn unquote(value: &str) -> &str {
    value
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
        .or_else(|| {
            value
                .strip_prefix('"')
                .and_then(|rest| rest.strip_suffix('"'))
        })
        .unwrap_or(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static SEQUENCE: AtomicU32 = AtomicU32::new(0);

    struct Board(PathBuf);

    impl Board {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "dock-board-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(root.join("kanban/tasks")).unwrap();
            Self(root)
        }

        fn task(&self, name: &str, body: &str) -> &Self {
            fs::write(self.0.join("kanban/tasks").join(name), body).unwrap();
            self
        }
    }

    impl Drop for Board {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// The exact shape this repository's own task files use.
    fn task_file(id: u64, title: &str, status: &str) -> String {
        format!(
            "---\nid: {id}\ntitle: '{title}'\nstatus: {status}\npriority: high\ncreated: 2026-08-21T12:55:48+10:00\ntags:\n    - runtime\n    - tui\ndepends_on:\n    - 11\nclass: standard\n---\n\n# Outcome\n\nSomething.\n"
        )
    }

    #[test]
    fn a_task_files_front_matter_becomes_a_task() {
        let board = Board::new();
        board.task(
            "001-a.md",
            &task_file(1, "Slice 6.2: real-agent launch", "review"),
        );
        let tasks = load(&board.0.join("kanban/tasks"));
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, 1);
        assert_eq!(tasks[0].title, "Slice 6.2: real-agent launch");
        assert_eq!(tasks[0].status, "review");
        assert_eq!(tasks[0].priority, "high");
    }

    #[test]
    fn list_items_in_the_front_matter_are_never_mistaken_for_fields() {
        // `tags` and `depends_on` hold indented `- value` items. A naive `split_once(':')` over
        // every line would read them as keys and could overwrite a real field.
        let board = Board::new();
        board.task("001-a.md", &task_file(7, "Has tags", "backlog"));
        let tasks = load(&board.0.join("kanban/tasks"));
        assert_eq!(tasks[0].id, 7);
        assert_eq!(tasks[0].status, "backlog");
    }

    #[test]
    fn tasks_are_ordered_by_where_the_work_is_rather_than_by_filename() {
        let board = Board::new();
        board
            .task("003-c.md", &task_file(3, "Done thing", "done"))
            .task("001-a.md", &task_file(1, "Backlog thing", "backlog"))
            .task("002-b.md", &task_file(2, "Running thing", "in-progress"));
        let statuses: Vec<String> = load(&board.0.join("kanban/tasks"))
            .into_iter()
            .map(|task| task.status)
            .collect();
        assert_eq!(statuses, ["in-progress", "backlog", "done"]);
    }

    #[test]
    fn an_unfamiliar_status_is_shown_last_rather_than_dropped() {
        let board = Board::new();
        board
            .task("001-a.md", &task_file(1, "Odd", "blocked"))
            .task("002-b.md", &task_file(2, "Known", "review"));
        let statuses: Vec<String> = load(&board.0.join("kanban/tasks"))
            .iter()
            .map(|task| task.status.clone())
            .collect();
        assert_eq!(statuses, ["review", "blocked"]);
    }

    #[test]
    fn one_malformed_task_does_not_hide_the_others() {
        let board = Board::new();
        board
            .task("001-a.md", "no front matter at all\n")
            .task("002-b.md", "---\ntitle: 'No id'\nstatus: review\n---\n")
            .task("003-c.md", &task_file(3, "Fine", "review"));
        let tasks = load(&board.0.join("kanban/tasks"));
        assert_eq!(tasks.len(), 1, "{tasks:?}");
        assert_eq!(tasks[0].id, 3);
    }

    #[test]
    fn a_created_task_reads_back_as_a_task() {
        let board = Board::new();
        let dir = board.0.join("kanban/tasks");
        let made = create(&dir, "Track the weather agent").expect("create");
        assert_eq!(made.id, 1);
        assert_eq!(made.status, "backlog");
        // The point of writing front matter rather than a private format: the file this produces
        // is the same shape kanban-md and every other reader already understands.
        let listed = load(&dir);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].title, "Track the weather agent");
        assert_eq!(listed[0].id, 1);
    }

    #[test]
    fn ids_continue_past_the_highest_present_rather_than_filling_gaps() {
        let board = Board::new();
        let dir = board.0.join("kanban/tasks");
        board.task("007-existing.md", &task_file(7, "Existing", "done"));
        // Reusing a gap would collide with a task that was archived rather than deleted, which is
        // a thing boards do.
        assert_eq!(create(&dir, "Next").expect("create").id, 8);
    }

    #[test]
    fn a_title_without_a_usable_slug_still_produces_a_readable_task() {
        let board = Board::new();
        let dir = board.0.join("kanban/tasks");
        let made = create(&dir, "!!! ???").expect("create");
        assert_eq!(load(&dir).len(), 1);
        assert_eq!(made.title, "!!! ???");
    }

    #[test]
    fn a_quote_in_a_title_cannot_break_the_front_matter_it_is_written_into() {
        let board = Board::new();
        let dir = board.0.join("kanban/tasks");
        create(&dir, "don't break the parser").expect("create");
        // Read back through the same parser every other reader uses: if the quoting were wrong the
        // task would come back truncated or not at all.
        let listed = load(&dir);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].title, "dont break the parser");
    }

    #[test]
    fn an_empty_title_is_refused_rather_than_written() {
        let board = Board::new();
        assert!(create(&board.0.join("kanban/tasks"), "   ").is_err());
    }

    #[test]
    fn a_board_outside_a_repository_is_the_personal_one() {
        // Without a repository the board still has to live somewhere, and it is the same place
        // regardless of which directory the dashboard was opened in.
        let personal = personal_tasks_dir().expect("HOME is set in the test environment");
        assert_eq!(tasks_dir("").as_deref(), Some(personal.as_path()));
        assert!(is_personal(&personal));
        // A repository's board is its own, and is never treated as Dock's to write to.
        let repository = tasks_dir("/repo/real").expect("a repository board");
        assert_eq!(repository, Path::new("/repo/real/kanban/tasks"));
        assert!(!is_personal(&repository));
    }

    #[test]
    fn a_repository_with_no_board_lists_nothing_rather_than_failing() {
        assert!(load(Path::new("/nonexistent-dock-board")).is_empty());
    }

    #[test]
    fn this_repositorys_own_board_parses() {
        // The format is not hypothetical: Dock's own tasks are the fixture.
        let tasks = load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("kanban/tasks"));
        assert!(!tasks.is_empty(), "Dock's own kanban/tasks must parse");
        assert!(tasks.iter().all(|task| task.id > 0));
        assert!(tasks.iter().all(|task| !task.title.is_empty()));
    }
}
