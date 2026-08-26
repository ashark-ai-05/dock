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
    /// Everything below the front matter: the Markdown the task was actually written in.
    ///
    /// The front matter is bookkeeping — an id, a column, a priority — and the title is a
    /// filename-length summary of it. What the work *is* is down here: the outcome, the
    /// acceptance criteria, what was ruled out. Dispatching a card sent an agent the title and
    /// nothing else, which asked it to do work nobody had described to it.
    pub body: String,
    /// Retired from the board without being deleted from the repository.
    ///
    /// The board had no terminal state: a card moved to `done` stayed in that column
    /// forever, because nothing prunes, expires or deletes. Absent means false, so every task
    /// file that predates this field — which is all of them — reads back exactly as before.
    pub archived: bool,
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
pub fn tasks_dir(repository_root: &str, workspace_id: &str) -> Option<PathBuf> {
    let repository = repository_root.trim();
    if !repository.is_empty() {
        return Some(Path::new(repository).join("kanban").join("tasks"));
    }
    workspace_tasks_dir(workspace_id)
}

/// A workspace's own board, under `~/.dock/boards/<workspace>/tasks`.
///
/// Per workspace rather than one board for everything, because a workspace is already the unit a
/// person keeps a piece of work in: its panes, its layout, and now its list. One shared list would
/// mix a side project's tasks into whatever is in front of you.
///
/// Keyed by workspace id rather than name: names are renameable, and a board that moved every time
/// its workspace was retitled would lose its tasks exactly when someone was tidying up.
pub fn workspace_tasks_dir(workspace_id: &str) -> Option<PathBuf> {
    let workspace = workspace_id.trim();
    let name = if workspace.is_empty() {
        "default".to_owned()
    } else {
        workspace
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                    character
                } else {
                    '-'
                }
            })
            .collect()
    };
    Some(dock_boards_dir()?.join(name).join("tasks"))
}

/// `~/.dock/boards`, the root every workspace board lives under.
pub fn dock_boards_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    if home.is_empty() {
        return None;
    }
    Some(PathBuf::from(home).join(".dock").join("boards"))
}

/// Whether this directory is one of Dock's own boards rather than a repository's.
///
/// Dock writes tasks only to its own. A repository's board is owned by `kanban-md` and by whoever
/// else commits to it, and creating files in someone else's board is not Dock's to do.
pub fn is_personal(directory: &Path) -> bool {
    dock_boards_dir().is_some_and(|root| directory.starts_with(root))
}

/// Every board Dock holds, as `(name, tasks directory)`, so one workspace can look at another's.
pub fn boards() -> Vec<(String, PathBuf)> {
    let Some(root) = dock_boards_dir() else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut found: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            (name, entry.path().join("tasks"))
        })
        .collect();
    found.sort();
    found
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
    // Built apart from the front matter it is written under, so what this returns is what `load`
    // will read back rather than a second description of the same file.
    let body = format!("# Outcome\n\n{safe_title}");
    let contents = format!(
        "---\nid: {id}\ntitle: '{safe_title}'\nstatus: backlog\npriority: medium\nclass: standard\n---\n\n{body}\n"
    );
    fs::write(&file, contents).map_err(|error| format!("could not write the task: {error}"))?;
    Ok(BoardTask {
        id,
        title: safe_title,
        status: "backlog".into(),
        priority: "medium".into(),
        file,
        body,
        archived: false,
    })
}

/// The statuses a task moves through, in the order work moves through them.
///
/// Not the whole truth about any particular board, and deliberately not treated as such: this
/// repository's own `kanban/config.yml` declares `needs-input`, which is not in here. Use
/// [`statuses`] for the columns a board actually has.
pub const STATUSES: [&str; 5] = ["backlog", "todo", "in-progress", "review", "done"];

/// The columns one board actually has: the statuses Dock knows, plus any the board itself uses.
///
/// The constant above and `kanban/config.yml` disagree, and everything that filtered by the
/// constant simply dropped the difference — a card a person had moved to `needs-input` by hand
/// was invisible in the column view and could not be moved back off it, even though `load` sorted
/// it perfectly well. Taking the union fixes that for whatever status a board uses rather than
/// for that one word, needs no second file format, and moves no column anybody is used to: the
/// known statuses keep their order and the unfamiliar ones are appended in the order
/// [`status_rank`] already sorts cards by, which is where an unfamiliar column already sorted.
pub fn statuses(tasks: &[BoardTask]) -> Vec<String> {
    let mut columns: Vec<String> = STATUSES.iter().map(|status| (*status).to_owned()).collect();
    let mut extra: Vec<&str> = tasks
        .iter()
        .map(|task| task.status.as_str())
        .filter(|status| !STATUSES.contains(status))
        .collect();
    extra.sort_by(|a, b| status_rank(a).cmp(&status_rank(b)).then_with(|| a.cmp(b)));
    extra.dedup();
    columns.extend(extra.into_iter().map(str::to_owned));
    columns
}

/// Moves a task to a new status, rewriting only that line of its front matter.
///
/// Only the `status:` line is touched. Everything else in the file — the body, the tags, whatever
/// another tool wrote there — is preserved byte for byte, because a board is shared with
/// `kanban-md`, with editors, and with whoever commits to it, and a tool that reformats other
/// people's files on the way past is a tool nobody keeps.
///
/// The destination is checked against [`statuses`] rather than the constant, so a column this
/// board plainly has is a place a card can be put; a typo is still refused, which is what the
/// check was for.
pub fn set_status(directory: &Path, id: u64, status: &str) -> Result<BoardTask, String> {
    let status = status.trim();
    let tasks = load(directory);
    let columns = statuses(&tasks);
    if !columns.iter().any(|known| known == status) {
        return Err(format!(
            "unknown status {status:?}; expected one of {}",
            columns.join(", ")
        ));
    }
    let task = tasks
        .into_iter()
        .find(|task| task.id == id)
        .ok_or_else(|| format!("no task {id} on this board"))?;
    let text = fs::read_to_string(&task.file)
        .map_err(|error| format!("could not read the task: {error}"))?;
    let mut rewritten = String::with_capacity(text.len());
    let mut in_front_matter = false;
    let mut replaced = false;
    for (index, line) in text.lines().enumerate() {
        if line.trim() == "---" {
            // The opening fence turns it on; the closing fence turns it off, so a `status:` in
            // the body below cannot be mistaken for the field.
            in_front_matter = index == 0 || !in_front_matter;
        }
        if in_front_matter && !replaced && line.starts_with("status:") {
            rewritten.push_str(&format!("status: {status}"));
            replaced = true;
        } else {
            rewritten.push_str(line);
        }
        rewritten.push('\n');
    }
    if !replaced {
        return Err(format!("task {id} has no status field to move"));
    }
    fs::write(&task.file, rewritten)
        .map_err(|error| format!("could not write the task: {error}"))?;
    Ok(BoardTask {
        status: status.to_owned(),
        ..task
    })
}

/// Retires a task from the board, or brings it back, rewriting only its `archived:` line.
///
/// Follows `set_status`'s shape and for the same reason — a board is shared with `kanban-md`,
/// with editors, and with whoever commits to it — with one difference: the field may not be
/// there at all, since every task written before this existed has no `archived:` line. So an
/// absent field is *inserted* immediately before the closing fence rather than treated as an
/// error, which is what `set_status` does with a missing `status:`.
pub fn set_archived(directory: &Path, id: u64, archived: bool) -> Result<BoardTask, String> {
    let task = load(directory)
        .into_iter()
        .find(|task| task.id == id)
        .ok_or_else(|| format!("no task {id} on this board"))?;
    let text = fs::read_to_string(&task.file)
        .map_err(|error| format!("could not read the task: {error}"))?;
    let mut rewritten = String::with_capacity(text.len() + 20);
    let mut in_front_matter = false;
    let mut replaced = false;
    for (index, line) in text.lines().enumerate() {
        let fence = line.trim() == "---";
        if fence && in_front_matter && !replaced {
            // The closing fence, and no field was found on the way here: put one in above it,
            // where the rest of the front matter is.
            rewritten.push_str(&format!("archived: {archived}\n"));
            replaced = true;
        }
        if fence {
            // The opening fence turns it on; the closing fence turns it off, so an `archived:`
            // in the body below cannot be mistaken for the field.
            in_front_matter = index == 0 || !in_front_matter;
        }
        if in_front_matter && !replaced && line.starts_with("archived:") {
            rewritten.push_str(&format!("archived: {archived}"));
            replaced = true;
        } else {
            rewritten.push_str(line);
        }
        rewritten.push('\n');
    }
    if !replaced {
        return Err(format!("task {id} has no front matter to archive"));
    }
    fs::write(&task.file, rewritten)
        .map_err(|error| format!("could not write the task: {error}"))?;
    Ok(BoardTask { archived, ..task })
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
///
/// The body needs no scanner at all: it is whatever follows the closing fence, trimmed. Which is
/// also why the fields stop at that fence rather than at the last one in the file — `---` is a
/// horizontal rule in Markdown, and a card that uses one would otherwise have its prose read as
/// front matter and everything above the rule read as no body.
fn parse(text: &str, path: &Path) -> Option<BoardTask> {
    // Lines with their terminators kept, so the offset the body starts at is a real byte offset
    // into `text` rather than a count that has lost every newline it walked past.
    let mut lines = text.split_inclusive('\n');
    let opening = lines.next()?;
    if opening.trim() != "---" {
        return None;
    }
    let (mut id, mut title, mut status, mut priority, mut archived) =
        (None, None, None, None, false);
    let mut body_start = text.len();
    let mut offset = opening.len();
    for line in lines {
        offset += line.len();
        if line.trim() == "---" {
            body_start = offset;
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
            "archived" => archived = value.eq_ignore_ascii_case("true"),
            _ => {}
        }
    }
    Some(BoardTask {
        id: id?,
        title: title.unwrap_or_default(),
        status: status.unwrap_or_else(|| "unknown".into()),
        priority: priority.unwrap_or_default(),
        file: path.to_path_buf(),
        body: text[body_start..].trim().to_owned(),
        archived,
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
    fn a_task_carries_the_markdown_its_author_wrote_under_the_front_matter() {
        // The front matter is bookkeeping; the body is the task. Dispatching sent an agent only
        // the title, so the acceptance criteria a person wrote under it never reached the work.
        let board = Board::new();
        board.task("001-a.md", &task_file(1, "Wire the parser", "backlog"));
        let tasks = load(&board.0.join("kanban/tasks"));
        assert_eq!(tasks[0].body, "# Outcome\n\nSomething.");
    }

    #[test]
    fn a_rule_in_the_body_is_body_rather_than_a_second_front_matter() {
        // `---` is a horizontal rule in Markdown. Reading the body as "everything after the last
        // fence" would drop the paragraph above it, and reading fields past the first closing
        // fence would let prose below overwrite a real field.
        let board = Board::new();
        board.task(
            "001-a.md",
            "---\nid: 4\ntitle: 'Ruled'\nstatus: review\n---\n\nAbove.\n\n---\n\nstatus: prose\n",
        );
        let tasks = load(&board.0.join("kanban/tasks"));
        assert_eq!(tasks[0].status, "review");
        assert_eq!(tasks[0].body, "Above.\n\n---\n\nstatus: prose");
    }

    #[test]
    fn a_card_with_nothing_under_its_front_matter_has_an_empty_body() {
        // Most cards on a personal board are a title and nothing else, and the empty string is
        // what tells a prompt builder there is nothing to add rather than a blank paragraph.
        let board = Board::new();
        board.task("001-a.md", "---\nid: 2\ntitle: 'Bare'\nstatus: done\n---\n");
        assert_eq!(load(&board.0.join("kanban/tasks"))[0].body, "");
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
        // What `create` hands back has to be what the file says, body included, or the task it
        // returns is a description of a file it did not write.
        assert_eq!(made.body, listed[0].body);
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
    fn each_workspace_gets_its_own_board_outside_a_repository() {
        // A workspace is already the unit a person keeps one piece of work in, so its list is
        // kept there too rather than pooled with every other workspace's.
        let one = tasks_dir("", "workspace_1").expect("HOME is set in the test environment");
        let two = tasks_dir("", "workspace_2").expect("a second board");
        assert_ne!(one, two);
        assert!(one.ends_with("workspace_1/tasks"), "{one:?}");
        assert!(is_personal(&one) && is_personal(&two));

        // A repository's board is its own, shared by every workspace open on it, and is never
        // treated as Dock's to write to.
        let repository = tasks_dir("/repo/real", "workspace_1").expect("a repository board");
        assert_eq!(repository, Path::new("/repo/real/kanban/tasks"));
        assert!(!is_personal(&repository));
    }

    #[test]
    fn a_workspace_id_that_is_not_a_filename_still_gets_a_board() {
        let awkward = tasks_dir("", "../../etc/passwd").expect("a board");
        // Nothing in the id may escape the boards directory: the id reaches this from the daemon,
        // and a board is a directory Dock creates.
        assert!(
            awkward.starts_with(dock_boards_dir().unwrap()),
            "{awkward:?}"
        );
        assert!(!awkward.to_string_lossy().contains(".."), "{awkward:?}");
    }

    #[test]
    fn moving_a_task_rewrites_its_status_and_nothing_else() {
        let board = Board::new();
        let dir = board.0.join("kanban/tasks");
        board.task("001-a.md", &task_file(1, "Wire the parser", "backlog"));
        let before = fs::read_to_string(dir.join("001-a.md")).unwrap();

        let moved = set_status(&dir, 1, "in-progress").expect("move");
        assert_eq!(moved.status, "in-progress");
        assert_eq!(load(&dir)[0].status, "in-progress");

        // A board is shared with kanban-md, with editors, and with whoever commits to it. Only
        // the one line may change; a tool that reformats other people's files is not one anybody
        // keeps pointed at their repository.
        let after = fs::read_to_string(dir.join("001-a.md")).unwrap();
        let changed: Vec<(&str, &str)> = before
            .lines()
            .zip(after.lines())
            .filter(|(a, b)| a != b)
            .collect();
        assert_eq!(changed, vec![("status: backlog", "status: in-progress")]);
        assert_eq!(before.lines().count(), after.lines().count());
    }

    #[test]
    fn an_unknown_status_is_refused_and_names_the_ones_that_exist() {
        let board = Board::new();
        let dir = board.0.join("kanban/tasks");
        board.task("001-a.md", &task_file(1, "Wire it", "backlog"));
        let refused = set_status(&dir, 1, "nearly-done").expect_err("refused");
        assert!(refused.contains("in-progress"), "{refused}");
        assert_eq!(load(&dir)[0].status, "backlog", "the task must not move");
    }

    #[test]
    fn a_status_the_board_already_uses_is_a_destination_the_constant_never_heard_of() {
        // `kanban/config.yml` declares `needs-input` and `STATUSES` does not. Refusing to move a
        // card into a column the board plainly has is the same defect as not drawing it.
        let board = Board::new();
        let dir = board.0.join("kanban/tasks");
        board
            .task("001-a.md", &task_file(1, "Wire it", "backlog"))
            .task(
                "002-b.md",
                &task_file(2, "Waiting on a person", "needs-input"),
            );
        assert_eq!(
            set_status(&dir, 1, "needs-input").expect("move").status,
            "needs-input"
        );
        // And a typo is still refused, which is the whole reason the check is there.
        let refused = set_status(&dir, 1, "needs-inpt").expect_err("refused");
        assert!(refused.contains("needs-input"), "{refused}");
    }

    #[test]
    fn moving_a_task_that_is_not_there_says_so() {
        let board = Board::new();
        assert!(set_status(&board.0.join("kanban/tasks"), 99, "done").is_err());
    }

    #[test]
    fn a_status_word_in_the_body_is_not_mistaken_for_the_field() {
        let board = Board::new();
        let dir = board.0.join("kanban/tasks");
        fs::write(
            dir.join("001-a.md"),
            "---\nid: 1\ntitle: 'Doc'\nstatus: backlog\n---\n\nstatus: this is prose\n",
        )
        .unwrap();
        set_status(&dir, 1, "done").expect("move");
        let after = fs::read_to_string(dir.join("001-a.md")).unwrap();
        assert!(after.contains("status: done"), "{after}");
        assert!(after.contains("status: this is prose"), "{after}");
    }

    #[test]
    fn a_repository_with_no_board_lists_nothing_rather_than_failing() {
        assert!(load(Path::new("/nonexistent-dock-board")).is_empty());
    }

    // ---------------------------------------------------------------------------------------
    // Load measurement.
    //
    // Not an assertion: `#[ignore]`d so `cargo test` never spends a second on it, and run
    // deliberately with
    //
    //     cargo test --release --lib -- --ignored --nocapture measure_board_load
    //
    // It exists because `BoardTask` gaining a `body` changed what `load` keeps: it used to read
    // every file and throw away everything under the front matter, so a card's length cost a
    // read and nothing else. Now the bodies are retained, and the board is loaded on every
    // refresh — so "does keeping them cost anything at a board size somebody actually has" is a
    // question to answer with numbers rather than with a shrug.
    //
    // The answer, measured against the same harness on the commit before the body was kept, was
    // no: fastest of six alternating runs, 335/1602/6360/6417µs against 333/1566/6281/6329µs for
    // the rows below. `load` costs about 32µs per task and is entirely per-file syscall; copying
    // a kilobyte of body out of a buffer already in hand is a rounding error beside opening the
    // file it came from, and a quarter-megabyte card does not show up either. The first pass at
    // this measurement said 11–15% slower, which was three other builds running on the machine.
    // ---------------------------------------------------------------------------------------

    /// A task file with roughly `body_bytes` of body under the front matter every card carries.
    fn bench_task_file(id: u64, body_bytes: usize) -> String {
        let paragraph =
            "Acceptance: the retry path stops after three attempts and says which one failed. ";
        let mut body = String::with_capacity(body_bytes + paragraph.len());
        while body.len() < body_bytes {
            body.push_str(paragraph);
        }
        format!(
            "---\nid: {id}\ntitle: 'Bench task {id}'\nstatus: backlog\npriority: medium\ncreated: 2026-08-21T12:55:48+10:00\ntags:\n    - runtime\n    - tui\nclass: standard\n---\n\n# Outcome\n\n{body}\n"
        )
    }

    /// A board of `tasks` cards, one of which may be far longer than the rest — which is what a
    /// real board looks like, since the one card somebody wrote a design into dwarfs the others.
    fn bench_board(tasks: usize, body_bytes: usize, one_giant: Option<usize>) -> Board {
        let board = Board::new();
        let directory = board.0.join("kanban/tasks");
        for id in 1..=tasks as u64 {
            let bytes = if id == 1 {
                one_giant.unwrap_or(body_bytes)
            } else {
                body_bytes
            };
            fs::write(
                directory.join(format!("{id:03}-bench.md")),
                bench_task_file(id, bytes),
            )
            .unwrap();
        }
        board
    }

    /// The fastest of several rounds rather than the mean.
    ///
    /// This machine's mean swings by tens of percent between identical runs depending on what
    /// else is on it, which is wide enough to hide a real regression — and has. The fastest
    /// round is the one that was interrupted least, which is the closest thing to the cost of
    /// the code rather than the cost of the afternoon.
    fn fastest_load(directory: &Path, rounds: u32) -> std::time::Duration {
        let mut fastest = std::time::Duration::MAX;
        for _ in 0..rounds {
            let start = std::time::Instant::now();
            let loaded = load(directory);
            let elapsed = start.elapsed();
            assert!(!loaded.is_empty(), "the bench board must have parsed");
            fastest = fastest.min(elapsed);
        }
        fastest
    }

    #[test]
    #[ignore = "a measurement, not an assertion: cargo test --release --lib -- --ignored --nocapture measure_board_load"]
    fn measure_board_load_across_the_board_sizes_people_actually_have() {
        println!();
        println!(
            "{:>8}  {:>12}  {:>12}  {:>12}",
            "tasks", "bodies", "total µs", "µs/task"
        );
        for (tasks, body_bytes, one_giant) in [
            (10usize, 1024usize, None),
            (50, 1024, None),
            (200, 1024, None),
            (200, 1024, Some(256 * 1024)),
        ] {
            let board = bench_board(tasks, body_bytes, one_giant);
            let fastest = fastest_load(&board.0.join("kanban/tasks"), 30);
            let micros = fastest.as_secs_f64() * 1e6;
            println!(
                "{tasks:>8}  {:>12}  {micros:>12.1}  {:>12.2}",
                match one_giant {
                    Some(bytes) => format!("1KB + {}KB", bytes / 1024),
                    None => "1KB".to_owned(),
                },
                micros / tasks as f64,
            );
        }
    }

    /// Archiving adds the field when the file has none and rewrites it when it has one, and in
    /// both cases leaves every other byte of the file alone.
    #[test]
    fn archiving_a_task_adds_or_rewrites_only_that_field() {
        let board = Board::new();
        board.task(
            "001-a.md",
            "---\nid: 1\ntitle: 'Thing'\nstatus: done\npriority: medium\ntags:\n  - keep\n---\n\n# Outcome\n\nbody text\n",
        );
        let dir = board.0.join("kanban/tasks");

        let archived = set_archived(&dir, 1, true).expect("archive");
        assert!(archived.archived);
        let text = fs::read_to_string(dir.join("001-a.md")).unwrap();
        assert!(text.contains("archived: true"), "{text}");
        assert!(
            text.contains("  - keep"),
            "the tags list must survive: {text}"
        );
        assert!(text.contains("body text"), "the body must survive: {text}");
        assert_eq!(
            text.matches("archived:").count(),
            1,
            "one field, not two: {text}"
        );

        set_archived(&dir, 1, false).expect("unarchive");
        let text = fs::read_to_string(dir.join("001-a.md")).unwrap();
        assert!(text.contains("archived: false"), "{text}");
        assert_eq!(text.matches("archived:").count(), 1, "{text}");
        assert!(
            !load(&dir)[0].archived,
            "and it reads back as visible again"
        );
    }

    #[test]
    fn this_repositorys_own_board_parses() {
        // The format is not hypothetical: Dock's own tasks are the fixture.
        let tasks = load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("kanban/tasks"));
        assert!(!tasks.is_empty(), "Dock's own kanban/tasks must parse");
        assert!(tasks.iter().all(|task| task.id > 0));
        assert!(tasks.iter().all(|task| !task.title.is_empty()));
        // And the body is where the work is actually described, on real files rather than on a
        // fixture written to make the parser look good.
        assert!(tasks.iter().any(|task| task.body.contains("Acceptance")));
    }
}

/// Where the cursor is on a board laid out as columns of cards.
///
/// Kept apart from the drawing so the awkward parts — an empty column, a column that empties when
/// its last card moves out of it, the edges — are decided by something that can be tested without
/// a terminal.
#[derive(Debug, Clone)]
pub struct BoardView {
    tasks: Vec<BoardTask>,
    /// The columns this board has, from [`statuses`]. Held rather than recomputed per call so
    /// every cursor rule agrees about how many columns there are.
    statuses: Vec<String>,
    column: usize,
    row: usize,
    /// Whether archived cards are shown. Off by default, so a board that has been archiving
    /// finished work looks like what is left to do rather than everything that was ever on it.
    reveal: bool,
}

impl Default for BoardView {
    /// A board with nothing on it still has the columns Dock knows, so a cursor on an empty
    /// board is a cursor somewhere rather than a cursor nowhere.
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl BoardView {
    /// Opens on the leftmost column that has anything in it, so a board whose backlog is empty
    /// does not open staring at nothing.
    pub fn new(tasks: Vec<BoardTask>) -> Self {
        let statuses = statuses(&tasks);
        let column = statuses
            .iter()
            .position(|status| tasks.iter().any(|task| task.status == *status))
            .unwrap_or(0);
        Self {
            tasks,
            statuses,
            column,
            row: 0,
            reveal: false,
        }
    }

    pub fn tasks(&self) -> &[BoardTask] {
        &self.tasks
    }

    /// The columns to draw, which is what to iterate instead of [`STATUSES`].
    pub fn statuses(&self) -> &[String] {
        &self.statuses
    }

    /// The status the cursor is in, or `None` for a board with no columns at all.
    pub fn status(&self) -> Option<&str> {
        self.statuses.get(self.column).map(String::as_str)
    }

    pub fn column(&self) -> usize {
        self.column
    }

    pub fn row(&self) -> usize {
        self.row
    }

    /// The cards in one column, in board order, minus anything archived unless revealed.
    ///
    /// Filtering *here* rather than at load is deliberate: `column_targets` builds the cursor's
    /// walk from this same call, so the cursor cannot disagree with the grid about how many
    /// cards a column has. A second filter anywhere else is how those two drift apart.
    pub fn cards(&self, status: &str) -> Vec<&BoardTask> {
        self.tasks
            .iter()
            .filter(|task| task.status == status)
            .filter(|task| self.reveal || !task.archived)
            .collect()
    }

    /// How many cards this column is holding back, revealed or not.
    pub fn archived_in(&self, status: &str) -> usize {
        self.tasks
            .iter()
            .filter(|task| task.status == status && task.archived)
            .count()
    }

    /// Shows or hides archived cards. `true` is what "revealed" means everywhere else on the
    /// board — the count in the footer, the muted card style — so a caller flips one bit.
    pub fn set_reveal(&mut self, reveal: bool) {
        self.reveal = reveal;
    }

    pub fn revealing(&self) -> bool {
        self.reveal
    }

    pub fn selected(&self) -> Option<&BoardTask> {
        self.cards(self.status()?).into_iter().nth(self.row)
    }

    /// Moves across columns, saturating rather than wrapping, and keeps the cursor on a card that
    /// exists — a taller column to the left leaves the row index past the end of a shorter one.
    pub fn move_column(&mut self, delta: isize) {
        self.column = self
            .column
            .saturating_add_signed(delta)
            .min(self.statuses.len().saturating_sub(1));
        self.clamp_row();
    }

    pub fn move_row(&mut self, delta: isize) {
        self.row = self.row.saturating_add_signed(delta);
        self.clamp_row();
    }

    fn clamp_row(&mut self) {
        let last = self
            .status()
            .map(|status| self.cards(status).len())
            .unwrap_or(0)
            .saturating_sub(1);
        self.row = self.row.min(last);
    }

    /// Follows a task to wherever it has just been moved, so the card the user was holding stays
    /// under the cursor instead of the cursor staying over a column position.
    pub fn follow(&mut self, id: u64) {
        if let Some(task) = self.tasks.iter().find(|task| task.id == id)
            && let Some(column) = self
                .statuses
                .iter()
                .position(|status| *status == task.status)
        {
            self.column = column;
            self.row = self
                .cards(&self.statuses[column])
                .iter()
                .position(|card| card.id == id)
                .unwrap_or(0);
        }
    }
}

#[cfg(test)]
mod view_tests {
    use super::*;

    fn task(id: u64, status: &str) -> BoardTask {
        task_with(id, status, false)
    }

    fn task_with(id: u64, status: &str, archived: bool) -> BoardTask {
        BoardTask {
            id,
            title: format!("task {id}"),
            status: status.into(),
            priority: "medium".into(),
            file: PathBuf::from(format!("{id}.md")),
            body: format!("# Outcome\n\ntask {id}"),
            archived,
        }
    }

    #[test]
    fn a_board_opens_on_the_first_column_holding_anything() {
        // Backlog and todo are empty; staring at an empty column would make a board with work on
        // it look like a board with none.
        let view = BoardView::new(vec![task(1, "in-progress"), task(2, "done")]);
        assert_eq!(STATUSES[view.column()], "in-progress");
        assert_eq!(view.selected().map(|task| task.id), Some(1));
    }

    /// A revealed board shows archived cards; a normal one does not, and says how many it is
    /// holding back.
    #[test]
    fn archived_cards_are_hidden_until_revealed_and_counted_while_they_are() {
        let mut view = BoardView::new(vec![
            task_with(1, "done", false),
            task_with(2, "done", true),
            task_with(3, "done", true),
        ]);
        assert_eq!(view.cards("done").len(), 1);
        assert_eq!(view.archived_in("done"), 2);
        view.set_reveal(true);
        assert_eq!(view.cards("done").len(), 3);
        assert_eq!(
            view.archived_in("done"),
            2,
            "the count is what is archived, revealed or not"
        );
    }

    #[test]
    fn moving_across_columns_saturates_and_keeps_the_cursor_on_a_card() {
        let mut view = BoardView::new(vec![
            task(1, "backlog"),
            task(2, "backlog"),
            task(3, "backlog"),
            task(4, "review"),
        ]);
        view.move_row(2);
        assert_eq!(view.selected().map(|task| task.id), Some(3));
        // Review holds one card, so a row index of 2 has to come back to something that exists.
        view.move_column(3);
        assert_eq!(STATUSES[view.column()], "review");
        assert_eq!(view.selected().map(|task| task.id), Some(4));
        // And the ends hold rather than wrapping round.
        view.move_column(9);
        assert_eq!(STATUSES[view.column()], "done");
        view.move_column(-9);
        assert_eq!(STATUSES[view.column()], "backlog");
    }

    #[test]
    fn an_empty_column_selects_nothing_instead_of_panicking() {
        let mut view = BoardView::new(vec![task(1, "backlog")]);
        view.move_column(1);
        assert!(view.selected().is_none());
        view.move_row(5);
        assert!(view.selected().is_none());
    }

    #[test]
    fn the_cursor_follows_a_card_that_moved_rather_than_staying_put() {
        let mut view = BoardView::new(vec![task(1, "backlog"), task(2, "backlog")]);
        view.move_row(1);
        assert_eq!(view.selected().map(|task| task.id), Some(2));
        // The card is moved on: the cursor should still be on it, in its new column.
        view.tasks[1].status = "in-progress".into();
        view.follow(2);
        assert_eq!(STATUSES[view.column()], "in-progress");
        assert_eq!(view.selected().map(|task| task.id), Some(2));
    }

    #[test]
    fn a_status_the_constant_does_not_know_is_still_a_column_the_cursor_can_reach() {
        // This repository's own `config.yml` declares `needs-input`, which `STATUSES` has never
        // heard of, and the view filtered by the constant — so a card a person had moved there by
        // hand was invisible on the board and could not be moved back off it either.
        let mut view = BoardView::new(vec![task(1, "backlog"), task(2, "needs-input")]);
        view.move_column(9);
        assert_eq!(view.status(), Some("needs-input"));
        assert_eq!(view.selected().map(|task| task.id), Some(2));
        // And back off it again, which is the half `<` could not do.
        view.move_column(-9);
        assert_eq!(view.status(), Some("backlog"));
        assert_eq!(view.selected().map(|task| task.id), Some(1));
    }

    #[test]
    fn unfamiliar_columns_are_appended_rather_than_reordering_the_board() {
        // The union must not move a column anybody is used to: the known statuses keep the order
        // work moves through them, and whatever else the board uses lands after them, in the
        // order `status_rank` already sorts cards by.
        let view = BoardView::new(vec![task(1, "needs-input"), task(2, "blocked")]);
        let columns: Vec<&str> = view.statuses().iter().map(String::as_str).collect();
        assert_eq!(
            columns,
            [
                "backlog",
                "todo",
                "in-progress",
                "review",
                "done",
                "blocked",
                "needs-input"
            ]
        );
    }

    #[test]
    fn an_empty_board_is_navigable_without_selecting_anything() {
        let mut view = BoardView::new(Vec::new());
        assert!(view.selected().is_none());
        view.move_column(2);
        view.move_row(2);
        assert!(view.selected().is_none());
    }
}
