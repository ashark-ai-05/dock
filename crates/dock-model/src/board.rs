//! Reading the markdown board straight from its task files.
//!
//! Tasks are Markdown with YAML front matter. Dock reads and writes them itself; there is no
//! companion binary for the board.

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
    /// The cards this one is waiting on, from `depends_on`.
    ///
    /// A card whose dependencies are not finished is not ready to pick up, and read like any
    /// other card it looks exactly as ready as one that is — which is the mistake a board is
    /// supposed to prevent.
    pub depends_on: Vec<u64>,
    /// Who claimed the card, from `claimed_by`. Absent means unclaimed.
    pub claimed_by: Option<String>,
    /// When the card was last touched, as seconds since the Unix epoch.
    ///
    /// From `updated` where the file has one and `created` otherwise. `None` for a card with
    /// neither, or with a stamp Dock could not read — an undated card is shown undated rather
    /// than shown as freshly touched, which would be a lie in the direction that hides work.
    pub touched: Option<i64>,
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
const STATUS_ORDER: [&str; 5] = ["in-progress", "needs-input", "review", "backlog", "done"];

/// Where the board a dashboard should show actually lives.
///
/// A repository's own board comes first: tasks belonging to a project live in its `kanban/tasks`.
///
/// Returns `None` when Dock is not sitting in a repository: there is no default under
/// `~/.dock/boards`. The board is markdown in the repo, or it is not a board Dock will open.
pub fn tasks_dir(repository_root: &str, _workspace_id: &str) -> Option<PathBuf> {
    resolve_tasks_dir(&[repository_root])
}

/// Where a dashboard should look for `kanban/tasks`.
///
/// `tasks_dir` used to answer from `repository_root` alone. That field is filled by the launch
/// catalog, which only ran when the launch form opened, so a `@board` pane in a repo that *has*
/// the directory sat on "reading the board…" forever. Walk every candidate — declared repo,
/// client cwd, pane cwd — and prefer a directory that actually exists. If none exists, keep the
/// historical contract: a non-empty declared root still points at `kanban/tasks` so a missing
/// board is an empty view rather than a silent refusal to look.
pub fn resolve_tasks_dir(candidates: &[&str]) -> Option<PathBuf> {
    let mut declared = None;
    let mut git_fallback = None;
    for candidate in candidates {
        let start = candidate.trim();
        if start.is_empty() {
            continue;
        }
        for ancestor in Path::new(start).ancestors() {
            let tasks = ancestor.join("kanban").join("tasks");
            if tasks.is_dir() {
                return Some(tasks);
            }
            if declared.is_none() {
                declared = Some(Path::new(start).join("kanban").join("tasks"));
            }
            if git_fallback.is_none() && ancestor.join(".git").exists() {
                git_fallback = Some(tasks);
            }
        }
    }
    git_fallback.or(declared)
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

/// Whether this directory is a leftover personal board under `~/.dock/boards`.
///
/// Repo markdown is the store Dock writes. This helper only identifies the old personal path;
/// it does not gate writes (the overlay is writable on a repository board).
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
/// Dock writes the markdown board in the repo. The id is one past the highest already present,
/// so it stays stable against a board other tools also write to: reusing a free gap would collide
/// with a task that was archived rather than deleted.
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
    let stamp = rfc3339_now();
    // Single quotes around the title, matching the front matter these files already use, with any
    // quote of its own removed rather than escaped: a title is not worth a YAML quoting dialect.
    let safe_title = title.replace('\'', "");
    // Built apart from the front matter it is written under, so what this returns is what `load`
    // will read back rather than a second description of the same file.
    let body = format!("# Outcome\n\n{safe_title}");
    let contents = format!(
        "---\nid: {id}\ntitle: '{safe_title}'\nstatus: backlog\npriority: medium\nclass: standard{}\n---\n\n{body}\n",
        stamp
            .as_ref()
            .map(|(_, text)| format!("\ncreated: {text}"))
            .unwrap_or_default()
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
        touched: stamp.map(|(seconds, _)| seconds),
        depends_on: Vec::new(),
        claimed_by: None,
    })
}

/// The statuses a task moves through on a board that has not declared its own.
///
/// One list, shared with [`crate::board_config::BoardConfig::default`], because two defaults
/// that disagree is how `todo` came to be drawn on every board while no kanban-md config
/// declared it. A board with a `config.yml` still overrides this through [`statuses_declaring`];
/// use [`statuses`] for the columns a board actually has.
pub const STATUSES: [&str; 5] = crate::board_config::KANBAN_MD_STATUSES;

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
    statuses_declaring(tasks, &STATUSES.map(str::to_owned))
}

/// The columns a board has, given what it *declares* it has.
///
/// `declared` is the board's own `config.yml` list where there is one, and Dock's constant where
/// there is not. Taking it from the board is the point: this repository declares
/// `backlog, in-progress, needs-input, review, done` and Dock's constant declares
/// `backlog, todo, in-progress, review, done`, so every board drew a `TODO` column that exists
/// nowhere but in Dock's source, and could never move a card into `needs-input` — the one column
/// an agent workflow most needs, because it is where an agent parks something it cannot finish
/// without you.
///
/// A status a board *uses* but does not declare is still appended, exactly as before: a column
/// somebody put a card in by hand must be visible and reachable, whatever the config says.
pub fn statuses_declaring(tasks: &[BoardTask], declared: &[String]) -> Vec<String> {
    let mut columns: Vec<String> = declared.to_vec();
    let mut extra: Vec<&str> = tasks
        .iter()
        .map(|task| task.status.as_str())
        .filter(|status| !columns.iter().any(|known| known == status))
        .collect();
    extra.sort_by(|a, b| status_rank(a).cmp(&status_rank(b)).then_with(|| a.cmp(b)));
    extra.dedup();
    columns.extend(extra.into_iter().map(str::to_owned));
    columns
}

/// Moves a task to a new status, rewriting only that line of its front matter.
///
/// Only the `status:` line is touched. Everything else in the file — the body, the tags, whatever
/// another tool wrote there — is preserved byte for byte, because a board is shared with editors
/// and whoever commits to it, and a tool that reformats other people's files on the way past is a
/// tool nobody keeps.
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
    if let Some(limit) = crate::board_config::load(directory).wip_limit_for(status) {
        let occupied = tasks
            .iter()
            .filter(|task| task.status == status && !task.archived && task.id != id)
            .count();
        if occupied >= limit {
            return Err(format!("WIP limit {limit} already filled in {status}"));
        }
    }
    if status == "in-progress"
        && let Some(task) = tasks.iter().find(|task| task.id == id)
    {
        for dep in &task.depends_on {
            if let Some(other) = tasks.iter().find(|other| other.id == *dep)
                && other.status != "done"
                && !other.archived
            {
                return Err(format!(
                    "task {id} depends on {dep} still in {}",
                    other.status
                ));
            }
        }
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
/// Follows `set_status`'s shape and for the same reason — a board is shared with editors and
/// whoever commits to it — with one difference: the field may not be
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

/// Whether this column is the one a ready card sits in before anyone claims it.
///
/// `backlog` is the usual name. `todo` is accepted as the same thing: some boards still use it,
/// and a card sitting there is not less ready than one sitting in `backlog`.
pub fn is_ready_status(status: &str) -> bool {
    matches!(status.trim(), "backlog" | "todo")
}

fn dependency_is_clear(tasks: &[BoardTask], dep: u64) -> bool {
    match tasks.iter().find(|task| task.id == dep) {
        // Nothing left on the board to wait for — the same reading `set_status` uses.
        None => true,
        Some(other) => other.status == "done" || other.archived,
    }
}

fn wip_allows_in_progress(directory: &Path, tasks: &[BoardTask]) -> bool {
    match crate::board_config::load(directory).wip_limit_for("in-progress") {
        None => true,
        Some(limit) => {
            tasks
                .iter()
                .filter(|task| task.status == "in-progress" && !task.archived)
                .count()
                < limit
        }
    }
}

fn priority_rank(priority: &str) -> u8 {
    match priority.trim() {
        "critical" => 0,
        "high" => 1,
        "medium" | "" => 2,
        "low" => 3,
        _ => 4,
    }
}

/// A card that can be claimed: not archived, in backlog (or `todo`), every `depends_on` done or
/// archived, and the in-progress column still has room.
pub fn is_ready(directory: &Path, task: &BoardTask, tasks: &[BoardTask]) -> bool {
    !task.archived
        && is_ready_status(&task.status)
        && task
            .depends_on
            .iter()
            .all(|dep| dependency_is_clear(tasks, *dep))
        && wip_allows_in_progress(directory, tasks)
}

/// The next ready card, highest priority first, then lowest id.
pub fn next_ready(directory: &Path) -> Option<BoardTask> {
    let tasks = load(directory);
    if !wip_allows_in_progress(directory, &tasks) {
        return None;
    }
    let mut ready: Vec<BoardTask> = tasks
        .iter()
        .filter(|task| is_ready(directory, task, &tasks))
        .cloned()
        .collect();
    ready.sort_by_key(|task| (priority_rank(&task.priority), task.id));
    ready.into_iter().next()
}

/// Claims the next ready card and returns it.
pub fn claim_next(directory: &Path, who: &str) -> Result<BoardTask, String> {
    let task = next_ready(directory).ok_or_else(|| "no ready task".to_owned())?;
    set_status(directory, task.id, "in-progress")?;
    set_claimed_by(directory, task.id, who)
}

/// Records who claimed the card, rewriting only `claimed_by`.
pub fn set_claimed_by(directory: &Path, id: u64, claim: &str) -> Result<BoardTask, String> {
    let claim = claim.trim();
    if claim.is_empty() {
        return Err("a claim needs a name".into());
    }
    let task = load(directory)
        .into_iter()
        .find(|task| task.id == id)
        .ok_or_else(|| format!("no task {id} on this board"))?;
    let text = fs::read_to_string(&task.file)
        .map_err(|error| format!("could not read the task: {error}"))?;
    let mut rewritten = String::with_capacity(text.len() + 24);
    let mut in_front_matter = false;
    let mut replaced = false;
    for (index, line) in text.lines().enumerate() {
        let fence = line.trim() == "---";
        if fence && in_front_matter && !replaced {
            rewritten.push_str(&format!("claimed_by: {claim}\n"));
            replaced = true;
        }
        if fence {
            in_front_matter = index == 0 || !in_front_matter;
        }
        if in_front_matter && !replaced && line.starts_with("claimed_by:") {
            rewritten.push_str(&format!("claimed_by: {claim}"));
            replaced = true;
        } else {
            rewritten.push_str(line);
        }
        rewritten.push('\n');
    }
    if !replaced {
        return Err(format!("task {id} has no front matter to claim"));
    }
    fs::write(&task.file, rewritten)
        .map_err(|error| format!("could not write the task: {error}"))?;
    let mut claimed = task;
    claimed.claimed_by = Some(claim.to_owned());
    Ok(claimed)
}

/// Latest mtime of the tasks directory or a file in it, so the TUI can reload without a key.
pub fn directory_mtime(directory: &Path) -> Option<std::time::SystemTime> {
    let mut latest = directory
        .metadata()
        .ok()
        .and_then(|meta| meta.modified().ok());
    if let Ok(entries) = fs::read_dir(directory) {
        for entry in entries.flatten() {
            if let Ok(modified) = entry.metadata().and_then(|meta| meta.modified()) {
                latest = Some(latest.map_or(modified, |current| current.max(modified)));
            }
        }
    }
    latest
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
    let mut claimed_by = None;
    let (mut created, mut updated) = (None, None);
    let mut depends_on: Vec<u64> = Vec::new();
    // Which list, if any, the indented lines below currently belong to. The front matter also
    // carries `tags`, whose items are words rather than ids, so the key has to be remembered
    // rather than every indented `- item` being read as a dependency.
    let mut list: Option<&str> = None;
    let mut body_start = text.len();
    let mut offset = opening.len();
    for line in lines {
        offset += line.len();
        if line.trim() == "---" {
            body_start = offset;
            break;
        }
        if line.starts_with(char::is_whitespace) {
            if list == Some("depends_on")
                && let Some(item) = line.trim().strip_prefix("- ")
                && let Ok(id) = unquote(item.trim()).parse::<u64>()
            {
                depends_on.push(id);
            }
            continue;
        }
        // An unindented line ends whatever list was open, whether or not it opens another.
        list = line.split_once(':').map(|(key, _)| key.trim());
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
            "claimed_by" if !value.is_empty() => claimed_by = Some(value.to_owned()),
            "created" => created = rfc3339_seconds(value),
            "updated" => updated = rfc3339_seconds(value),
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
        touched: updated.or(created),
        depends_on,
        claimed_by,
    })
}

/// Seconds since the Unix epoch, from an RFC 3339 stamp like kanban-md writes.
///
/// `2026-08-21T08:04:40+10:00` and `2026-08-21T02:32:41Z` are both the shape these files use.
/// Parsed rather than pulled in as a dependency, for the same reason the front matter around it
/// is: the format is fixed, only one field is wanted, and a malformed stamp must cost that one
/// card its age rather than the whole board its render.
///
/// Returns `None` for anything it does not fully understand, which the caller treats as "this
/// card has no age" — a card that cannot be dated is better shown undated than shown wrong.
fn rfc3339_seconds(value: &str) -> Option<i64> {
    let value = unquote(value.trim());
    let (date, rest) = value.split_once('T')?;
    let mut date = date.split('-');
    let year: i64 = date.next()?.parse().ok()?;
    let month: i64 = date.next()?.parse().ok()?;
    let day: i64 = date.next()?.parse().ok()?;
    // The offset is whatever follows the time, and its sign decides which way to correct.
    let (clock, offset) = match rest.find(['Z', 'z', '+']) {
        Some(index) => rest.split_at(index),
        // A `-` inside the time is impossible, so the last one can only start a negative offset.
        None => match rest.rfind('-') {
            Some(index) => rest.split_at(index),
            None => (rest, ""),
        },
    };
    let mut clock = clock.split(':');
    let hour: i64 = clock.next()?.parse().ok()?;
    let minute: i64 = clock.next()?.parse().ok()?;
    // Seconds may carry a fraction, which is precision this does not need.
    let second: i64 = clock
        .next()
        .unwrap_or("0")
        .split('.')
        .next()?
        .parse()
        .ok()?;
    let offset_seconds = match offset.chars().next() {
        None | Some('Z' | 'z') => 0,
        Some(sign) => {
            let mut parts = offset[1..].split(':');
            let hours: i64 = parts.next()?.parse().ok()?;
            let minutes: i64 = parts.next().unwrap_or("0").parse().ok()?;
            let magnitude = hours * 3_600 + minutes * 60;
            if sign == '-' { -magnitude } else { magnitude }
        }
    };
    // Days from the civil date, by the shift-the-year-to-March algorithm: with March as month
    // one, the leap day lands at the end of the year and the month-length pattern repeats every
    // five months, which is what makes the whole thing a handful of integer operations with no
    // table and no branches.
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second - offset_seconds)
}

/// The current instant as the RFC 3339 UTC stamp these files carry.
///
/// The inverse of [`rfc3339_seconds`], and written for the same reason: a card Dock creates
/// must age like a card kanban-md creates, and an undated card would sit at whatever the board
/// calls "fresh" forever — the one direction of error that hides work rather than surfacing it.
fn rfc3339_now() -> Option<(i64, String)> {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    // Civil date from days, the mirror of the shift-to-March arithmetic in `rfc3339_seconds`.
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_shifted = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_shifted + 2) / 5 + 1;
    let month = month_shifted + if month_shifted < 10 { 3 } else { -9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    Some((
        seconds,
        format!(
            "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
            rest / 3_600,
            (rest % 3_600) / 60,
            rest % 60
        ),
    ))
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

    /// `depends_on` is read; `tags`, which sits beside it in the same shape, is not mistaken
    /// for it.
    #[test]
    fn a_cards_dependencies_are_read_and_its_tags_are_not() {
        let board = Board::new();
        board.task(
            "001-a.md",
            "---\nid: 1\ntitle: 'Blocked'\nstatus: backlog\ntags:\n    - runtime\n    - tui\ndepends_on:\n    - 10\n    - 11\nclass: standard\n---\n\n# Outcome\n\nbody\n",
        );
        let tasks = load(&board.0.join("kanban/tasks"));
        assert_eq!(
            tasks[0].depends_on,
            [10, 11],
            "both dependencies, and nothing from `tags`"
        );
    }

    /// A card with no `depends_on` waits on nothing, rather than on whatever list came last.
    #[test]
    fn a_card_with_no_dependencies_waits_on_nothing() {
        let board = Board::new();
        board.task(
            "001-a.md",
            "---\nid: 1\ntitle: 'Free'\nstatus: backlog\ntags:\n    - runtime\n---\n\n# Outcome\n\nbody\n",
        );
        let tasks = load(&board.0.join("kanban/tasks"));
        assert!(tasks[0].depends_on.is_empty(), "{:?}", tasks[0].depends_on);
    }

    /// Both stamp shapes these files actually use, checked against known epoch seconds.
    #[test]
    fn an_rfc3339_stamp_reads_as_epoch_seconds() {
        assert_eq!(rfc3339_seconds("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(rfc3339_seconds("2026-08-21T02:32:41Z"), Some(1_787_279_561));
        // The same instant, written with an offset instead of `Z`.
        assert_eq!(
            rfc3339_seconds("2026-08-21T12:32:41+10:00"),
            Some(1_787_279_561),
            "an offset is corrected for, not ignored"
        );
        assert_eq!(
            rfc3339_seconds("2026-08-20T21:32:41-05:00"),
            Some(1_787_279_561),
            "in both directions"
        );
        assert_eq!(
            rfc3339_seconds("2026-08-21T02:32:41.123Z"),
            Some(1_787_279_561),
            "a fraction is precision this does not need"
        );
    }

    /// A stamp Dock cannot read costs that card its age, not the board its render.
    #[test]
    fn an_unreadable_stamp_is_no_age_rather_than_a_wrong_one() {
        assert_eq!(rfc3339_seconds("yesterday"), None);
        assert_eq!(rfc3339_seconds("2026-08-21"), None);
        assert_eq!(rfc3339_seconds(""), None);
    }

    /// End to end: a board that declares its own columns gets them, phantom column and all.
    ///
    /// `TODO` existed only in Dock's `STATUSES` constant. Every board drew it, empty, forever —
    /// and `needs-input`, which this repository's config really does declare, could not be
    /// reached at all, because a column only appeared once a card was already in it and `>`
    /// could not move one there.
    #[test]
    fn a_board_that_declares_its_columns_gets_them_and_not_docks() {
        let board = Board::new();
        std::fs::write(
            board.0.join("kanban/config.yml"),
            "tasks_dir: tasks\nstatuses:\n    - name: backlog\n    - name: in-progress\n    - name: needs-input\n    - name: review\n    - name: done\ntui:\n    title_lines: 2\n",
        )
        .unwrap();
        board.task("001-a.md", &task_file(1, "A card", "backlog"));
        let dir = board.0.join("kanban/tasks");

        let config = crate::board_config::load(&dir);
        let view = BoardView::with_config(load(&dir), &config);

        assert_eq!(
            view.statuses(),
            ["backlog", "in-progress", "needs-input", "review"],
            "the board's own columns, in its own order, without done until revealed"
        );
        let mut revealed = view.clone();
        revealed.set_reveal(true);
        assert_eq!(
            revealed.statuses(),
            ["backlog", "in-progress", "needs-input", "review", "done"],
        );
        assert!(
            !view.statuses().contains(&"todo"),
            "the column that existed only in Dock is gone: {:?}",
            view.statuses()
        );
        assert_eq!(
            view.title_lines(),
            2,
            "and the card shape the board asked for came with them"
        );
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
    fn a_board_lives_in_the_repository_or_nowhere() {
        assert!(
            tasks_dir("", "workspace_1").is_none(),
            "no ~/.dock/boards default"
        );
        let repository = tasks_dir("/repo/real", "workspace_1").expect("a repository board");
        assert_eq!(repository, Path::new("/repo/real/kanban/tasks"));
        assert!(!is_personal(&repository));
    }

    #[test]
    fn an_existing_kanban_on_cwd_is_found_when_the_declared_root_is_empty() {
        let root = std::env::temp_dir().join(format!(
            "dock-resolve-board-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let nested = root.join("src");
        let tasks = root.join("kanban").join("tasks");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(&tasks).unwrap();
        let found = resolve_tasks_dir(&["", nested.to_str().unwrap()]).expect("walk cwd");
        assert_eq!(found, tasks);
        assert!(resolve_tasks_dir(&["", "", "   "]).is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_workspace_id_that_is_not_a_filename_still_gets_a_personal_path_if_asked() {
        let awkward = workspace_tasks_dir("../../etc/passwd").expect("a board");
        assert!(
            awkward.starts_with(dock_boards_dir().unwrap()),
            "{awkward:?}"
        );
        assert!(!awkward.to_string_lossy().contains(".."), "{awkward:?}");
    }

    #[test]
    fn a_wip_limit_refuses_a_move_into_a_full_column() {
        let board = Board::new();
        let dir = board.0.join("kanban/tasks");
        fs::create_dir_all(dir.parent().unwrap()).unwrap();
        fs::write(
            dir.parent().unwrap().join("config.yml"),
            "statuses:\n    - name: backlog\n    - name: in-progress\n      wip_limit: 1\n    - name: done\n",
        )
        .unwrap();
        board.task("001-a.md", &task_file(1, "One", "in-progress"));
        board.task("002-b.md", &task_file(2, "Two", "backlog"));
        let refused = set_status(&dir, 2, "in-progress").expect_err("wip");
        assert!(refused.contains("WIP"), "{refused}");
        assert_eq!(
            load(&dir).iter().find(|t| t.id == 2).unwrap().status,
            "backlog"
        );
    }

    #[test]
    fn next_ready_picks_the_highest_priority_unblocked_backlog_card() {
        let board = Board::new();
        let dir = board.0.join("kanban/tasks");
        board.task(
            "001-a.md",
            "---\nid: 1\ntitle: 'Low'\nstatus: backlog\npriority: low\n---\n",
        );
        board.task(
            "002-b.md",
            "---\nid: 2\ntitle: 'Blocked'\nstatus: backlog\npriority: high\ndepends_on:\n  - 1\n---\n",
        );
        board.task(
            "003-c.md",
            "---\nid: 3\ntitle: 'High'\nstatus: backlog\npriority: high\n---\n",
        );
        board.task(
            "004-d.md",
            "---\nid: 4\ntitle: 'Archived'\nstatus: backlog\npriority: critical\narchived: true\n---\n",
        );
        board.task(
            "005-e.md",
            "---\nid: 5\ntitle: 'Review'\nstatus: review\npriority: critical\n---\n",
        );
        let next = next_ready(&dir).expect("a ready card");
        assert_eq!(next.id, 3);
        assert_eq!(next.title, "High");
    }

    #[test]
    fn a_todo_card_is_ready_the_way_a_backlog_card_is() {
        let board = Board::new();
        let dir = board.0.join("kanban/tasks");
        board.task(
            "001-a.md",
            "---\nid: 1\ntitle: 'Todo'\nstatus: todo\npriority: medium\n---\n",
        );
        assert_eq!(next_ready(&dir).map(|task| task.id), Some(1));
    }

    #[test]
    fn a_done_or_archived_dependency_unblocks_the_card() {
        let board = Board::new();
        let dir = board.0.join("kanban/tasks");
        board.task("001-a.md", "---\nid: 1\ntitle: 'A'\nstatus: done\n---\n");
        board.task(
            "002-b.md",
            "---\nid: 2\ntitle: 'B'\nstatus: backlog\narchived: true\n---\n",
        );
        board.task(
            "003-c.md",
            "---\nid: 3\ntitle: 'C'\nstatus: backlog\ndepends_on:\n  - 1\n  - 2\n---\n",
        );
        assert_eq!(next_ready(&dir).map(|task| task.id), Some(3));
    }

    #[test]
    fn a_full_wip_column_means_nothing_is_ready() {
        let board = Board::new();
        let dir = board.0.join("kanban/tasks");
        fs::create_dir_all(dir.parent().unwrap()).unwrap();
        fs::write(
            dir.parent().unwrap().join("config.yml"),
            "statuses:\n    - name: backlog\n    - name: in-progress\n      wip_limit: 1\n    - name: done\n",
        )
        .unwrap();
        board.task(
            "001-a.md",
            "---\nid: 1\ntitle: 'Busy'\nstatus: in-progress\n---\n",
        );
        board.task(
            "002-b.md",
            "---\nid: 2\ntitle: 'Waiting'\nstatus: backlog\n---\n",
        );
        assert!(next_ready(&dir).is_none());
    }

    #[test]
    fn claim_next_moves_the_ready_card_to_in_progress() {
        let board = Board::new();
        let dir = board.0.join("kanban/tasks");
        board.task(
            "001-a.md",
            "---\nid: 1\ntitle: 'Do it'\nstatus: backlog\npriority: high\n---\n",
        );
        let claimed = claim_next(&dir, "dock").expect("claim");
        assert_eq!(claimed.id, 1);
        assert_eq!(claimed.status, "in-progress");
        assert_eq!(claimed.claimed_by.as_deref(), Some("dock"));
        assert_eq!(load(&dir)[0].status, "in-progress");
    }

    #[test]
    fn unfinished_depends_on_block_a_claim() {
        let board = Board::new();
        let dir = board.0.join("kanban/tasks");
        board.task("001-a.md", "---\nid: 1\ntitle: 'A'\nstatus: backlog\n---\n");
        board.task(
            "002-b.md",
            "---\nid: 2\ntitle: 'B'\nstatus: backlog\ndepends_on:\n  - 1\n---\n",
        );
        let refused = set_status(&dir, 2, "in-progress").expect_err("depends");
        assert!(refused.contains("depends"), "{refused}");
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

        // Only the one line may change; a tool that reformats other people's files is not one
        // anybody keeps pointed at their repository.
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

    /// A `---` in the *body* is a Markdown horizontal rule, not a fence, and archiving must not
    /// treat it as one.
    ///
    /// `set_archived` is correct here for a reason nothing else pins: the body rule flips
    /// `in_front_matter` back to `true`, but `replaced` is already `true` by then, so neither
    /// the insert branch nor the rewrite branch can fire a second time. That is a two-variable
    /// argument holding a file the user's repository owns together, and it deserves better than
    /// a hand-trace in review.
    ///
    /// Asserted as the whole rewritten file against an expected string rather than as four
    /// `contains` checks, because "rewrites only the line it means to touch" is a claim about
    /// every other byte — and `contains` cannot see a byte that moved, doubled, or vanished.
    #[test]
    fn a_horizontal_rule_in_the_body_is_not_mistaken_for_the_front_matters_fence() {
        let board = Board::new();
        let dir = board.0.join("kanban/tasks");
        board.task(
            "001-a.md",
            concat!(
                "---\n",
                "id: 1\n",
                "title: 'Thing'\n",
                "status: done\n",
                "archived: false\n",
                "tags:\n",
                "  - keep\n",
                "---\n",
                "\n",
                "# Outcome\n",
                "\n",
                "before the rule\n",
                "\n",
                "---\n",
                "\n",
                "after the rule\n",
                "\n",
                "archived: true\n",
            ),
        );

        set_archived(&dir, 1, true).expect("archive");
        assert_eq!(
            fs::read_to_string(dir.join("001-a.md")).unwrap(),
            concat!(
                "---\n",
                "id: 1\n",
                "title: 'Thing'\n",
                "status: done\n",
                // The one line that changed.
                "archived: true\n",
                "tags:\n",
                "  - keep\n",
                "---\n",
                "\n",
                "# Outcome\n",
                "\n",
                "before the rule\n",
                "\n",
                // The rule survives as a rule: no `archived:` line was inserted above it, which
                // is what would happen if it were read as a closing fence.
                "---\n",
                "\n",
                "after the rule\n",
                "\n",
                // And prose that merely looks like the field is prose. Rewriting this would be
                // Dock editing the user's sentences.
                "archived: true\n",
            ),
        );

        // The same file, back the other way, so the insert branch and the rewrite branch are
        // both walked past the rule rather than only one of them.
        set_archived(&dir, 1, false).expect("unarchive");
        let text = fs::read_to_string(dir.join("001-a.md")).unwrap();
        assert_eq!(
            text.matches("archived: false").count(),
            1,
            "one field, and it is the one in the front matter: {text}"
        );
        assert!(
            text.ends_with("after the rule\n\narchived: true\n"),
            "the body is untouched: {text}"
        );
        assert!(!load(&dir)[0].archived);
    }

    #[test]
    fn this_repositorys_own_board_parses() {
        // The format is not hypothetical: Dock's own tasks are the fixture.
        //
        // `CARGO_MANIFEST_DIR` is this crate's own manifest directory
        // (`crates/dock-model`), not the workspace root, now that `board` lives in a
        // workspace crate rather than the root crate — hence the `../..`.
        let tasks = load(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../kanban/tasks")
                .canonicalize()
                .expect("workspace root's kanban/tasks must exist"),
        );
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
    /// How many lines a card's title may take, from the board's own `tui.title_lines`.
    title_lines: usize,
    /// The board's age rungs, oldest last. Empty when it declares none.
    age_thresholds: Vec<crate::board_config::AgeThreshold>,
    /// The columns this board has, from [`statuses`]. Held rather than recomputed per call so
    /// every cursor rule agrees about how many columns there are.
    statuses: Vec<String>,
    column: usize,
    row: usize,
    /// Whether archived cards *and* the `done` column are shown. Off by default, so a board
    /// that has been finishing work looks like what is left to do rather than an archive.
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
        Self::with_config(tasks, &crate::board_config::BoardConfig::default())
    }

    /// A view over a board that has told Dock what shape it is.
    ///
    /// The config carries the columns, how many lines a card's title may take, and the age rungs
    /// a stale card is coloured by — all of which the board already declared in `config.yml` and
    /// none of which reached the screen while Dock rendered from its own constants instead.
    pub fn with_config(tasks: Vec<BoardTask>, config: &crate::board_config::BoardConfig) -> Self {
        let statuses = statuses_declaring(&tasks, &config.statuses);
        // Membership by what would actually be visible, not by raw status: a column holding
        // only archived cards is empty to a board that opens un-revealed, and picking it as the
        // opening column left the cursor on a column `cards()` immediately reports as having
        // nothing in it — `a`, `Enter`, `<` and `>` all had nothing to act on.
        let column = statuses
            .iter()
            .position(|status| {
                tasks
                    .iter()
                    .any(|task| task.status == *status && !task.archived)
            })
            .unwrap_or(0);
        Self {
            tasks,
            statuses,
            title_lines: config.title_lines,
            age_thresholds: config.age_thresholds.clone(),
            column,
            row: 0,
            reveal: false,
        }
    }

    fn visible_statuses(&self) -> impl Iterator<Item = &str> {
        self.statuses
            .iter()
            .map(String::as_str)
            .filter(|status| self.reveal || *status != "done")
    }

    /// How many lines a card's title may take on this board.
    pub fn title_lines(&self) -> usize {
        self.title_lines
    }

    /// The board's age rungs, oldest last.
    pub fn age_thresholds(&self) -> &[crate::board_config::AgeThreshold] {
        &self.age_thresholds
    }

    pub fn tasks(&self) -> &[BoardTask] {
        &self.tasks
    }

    /// The columns to draw, which is what to iterate instead of [`STATUSES`].
    ///
    /// `done` is omitted unless revealed (`A`), so the default view is work.
    pub fn statuses(&self) -> Vec<&str> {
        self.visible_statuses().collect()
    }

    /// Every column the workflow can move a card through, including hidden `done`.
    pub fn workflow_statuses(&self) -> &[String] {
        &self.statuses
    }

    /// The status the cursor is in, or `None` for a board with no columns at all.
    pub fn status(&self) -> Option<&str> {
        self.visible_statuses().nth(self.column)
    }

    /// Whether any card is actually on screen in the default (or revealed) view.
    pub fn has_visible_cards(&self) -> bool {
        self.visible_statuses()
            .any(|status| !self.cards(status).is_empty())
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
        let last = self.statuses().len().saturating_sub(1);
        self.column = self.column.min(last);
        self.clamp_row();
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
            .min(self.statuses().len().saturating_sub(1));
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
        let Some(task) = self.tasks.iter().find(|task| task.id == id) else {
            return;
        };
        let status = task.status.clone();
        let Some(column) = self.visible_statuses().position(|shown| shown == status) else {
            return;
        };
        self.column = column;
        self.row = self
            .cards(&status)
            .iter()
            .position(|card| card.id == id)
            .unwrap_or(0);
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
            touched: None,
            depends_on: Vec::new(),
            claimed_by: None,
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

    /// A column whose only card is archived is not "holding anything" to a board that opens
    /// un-revealed: `cards()` reports it empty, so the opening cursor must skip past it to the
    /// next column that actually has something visible in it.
    #[test]
    fn an_all_archived_column_does_not_capture_the_opening_cursor() {
        let view = BoardView::new(vec![
            task_with(1, "backlog", true),
            task_with(2, "review", false),
        ]);
        assert_eq!(STATUSES[view.column()], "review");
        assert_eq!(view.selected().map(|task| task.id), Some(2));
    }

    /// A revealed board shows archived cards; a normal one does not, and says how many it is
    /// holding back.
    #[test]
    fn done_is_hidden_until_revealed() {
        let mut view = BoardView::new(vec![task(1, "backlog"), task(2, "done")]);
        assert!(!view.statuses().contains(&"done"));
        assert!(view.has_visible_cards());
        view.set_reveal(true);
        assert!(view.statuses().contains(&"done"));
        assert_eq!(view.cards("done").len(), 1);
    }

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
        assert_eq!(view.status(), Some("review"));
        view.set_reveal(true);
        view.move_column(9);
        assert_eq!(view.status(), Some("done"));
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
        // A status no default list knows. `blocked` is nobody's column: the view must still
        // show it and let the cursor reach it, or a card somebody moved there by hand would be
        // invisible on the board and impossible to move back off.
        let mut view = BoardView::new(vec![task(1, "backlog"), task(2, "blocked")]);
        view.move_column(9);
        assert_eq!(view.status(), Some("blocked"));
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
        let view = BoardView::new(vec![task(1, "todo"), task(2, "blocked")]);
        let columns: Vec<&str> = view.statuses();
        assert_eq!(
            columns,
            [
                "backlog",
                "in-progress",
                "needs-input",
                "review",
                "blocked",
                "todo",
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
