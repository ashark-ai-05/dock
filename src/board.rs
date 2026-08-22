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

/// Every task under `<repository_root>/kanban/tasks`, ordered by status then id.
///
/// Best-effort by design: a file that cannot be read or has no `id` is skipped rather than failing
/// the board, because one malformed task should not hide the other eleven.
pub fn load(repository_root: &Path) -> Vec<BoardTask> {
    let directory = repository_root.join("kanban").join("tasks");
    let Ok(entries) = fs::read_dir(&directory) else {
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
        let tasks = load(&board.0);
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
        let tasks = load(&board.0);
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
        let statuses: Vec<String> = load(&board.0).into_iter().map(|task| task.status).collect();
        assert_eq!(statuses, ["in-progress", "backlog", "done"]);
    }

    #[test]
    fn an_unfamiliar_status_is_shown_last_rather_than_dropped() {
        let board = Board::new();
        board
            .task("001-a.md", &task_file(1, "Odd", "blocked"))
            .task("002-b.md", &task_file(2, "Known", "review"));
        let statuses: Vec<String> = load(&board.0)
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
        let tasks = load(&board.0);
        assert_eq!(tasks.len(), 1, "{tasks:?}");
        assert_eq!(tasks[0].id, 3);
    }

    #[test]
    fn a_repository_with_no_board_lists_nothing_rather_than_failing() {
        assert!(load(Path::new("/nonexistent-dock-board")).is_empty());
    }

    #[test]
    fn this_repositorys_own_board_parses() {
        // The format is not hypothetical: Dock's own tasks are the fixture.
        let tasks = load(Path::new(env!("CARGO_MANIFEST_DIR")));
        assert!(!tasks.is_empty(), "Dock's own kanban/tasks must parse");
        assert!(tasks.iter().all(|task| task.id > 0));
        assert!(tasks.iter().all(|task| !task.title.is_empty()));
    }
}
