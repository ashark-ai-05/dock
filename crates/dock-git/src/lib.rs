use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub mod files;

/// Where a task's worktree lives and which branch it is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
    /// False when the worktree was already there and this call only found it. Dispatching twice to
    /// the same task is an ordinary thing to do, and the second time must not look like the first.
    pub created: bool,
}

/// Every worktree the repository knows about, as `(path, branch)`.
///
/// A detached worktree reports an empty branch rather than being omitted, because it still occupies
/// its path and a new worktree cannot be created there.
pub fn worktrees(repository_root: &Path) -> Result<Vec<(PathBuf, String)>, String> {
    let listing = run(repository_root, ["worktree", "list", "--porcelain"])?;
    let mut found = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut branch = String::new();
    for line in listing.lines() {
        if let Some(value) = line.strip_prefix("worktree ") {
            if let Some(previous) = path.take() {
                found.push((previous, std::mem::take(&mut branch)));
            }
            path = Some(PathBuf::from(value));
        } else if let Some(value) = line.strip_prefix("branch ") {
            branch = value.trim_start_matches("refs/heads/").to_owned();
        }
    }
    if let Some(last) = path {
        found.push((last, branch));
    }
    Ok(found)
}

/// Makes sure `branch` has a worktree, creating one at `path` if it does not already have one.
///
/// This is the one place Dock mutates a repository, and it is deliberately the least it can do to
/// give an agent somewhere isolated to work: it adds a worktree, and a branch when that branch does
/// not exist yet. It never stages, commits, rebases, merges, pushes, or removes anything, and it
/// never touches a path that is already occupied — an existing directory that is not this branch's
/// worktree is refused rather than reused, because whatever is in it belongs to someone else.
///
/// Idempotent on purpose. Dispatching the same task twice is ordinary, and the second dispatch
/// should land in the worktree the first one made rather than fail or make a second one.
pub fn ensure_worktree(
    repository_root: &Path,
    branch: &str,
    path: &Path,
    base: &str,
) -> Result<Worktree, String> {
    if branch.trim().is_empty() {
        return Err("a worktree needs a branch name".into());
    }
    // Already checked out somewhere: use it, whatever path was suggested.
    if let Some((existing, _)) = worktrees(repository_root)?
        .into_iter()
        .find(|(_, checked_out)| checked_out == branch)
    {
        return Ok(Worktree {
            path: existing,
            branch: branch.to_owned(),
            created: false,
        });
    }
    if path.exists() {
        return Err(format!(
            "{} already exists and is not a worktree of {branch}",
            path.display()
        ));
    }
    // `worktree add -b` creates the branch; without `-b` it checks out one that already exists.
    // Asking git which case applies is cheaper than parsing the failure of the wrong one.
    let branch_exists = run(
        repository_root,
        [
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .is_ok();
    let path_text = path.to_string_lossy().into_owned();
    if branch_exists {
        run(repository_root, ["worktree", "add", &path_text, branch])?;
    } else {
        run(
            repository_root,
            ["worktree", "add", "-b", branch, &path_text, base],
        )?;
    }
    let path = std::fs::canonicalize(path)
        .map_err(|error| format!("could not canonicalize the new worktree: {error}"))?;
    Ok(Worktree {
        path,
        branch: branch.to_owned(),
        created: true,
    })
}

fn run<const N: usize>(repository_root: &Path, arguments: [&str; N]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository_root)
        .args(arguments)
        .output()
        .map_err(|error| format!("could not run git: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitFacts {
    pub worktree: PathBuf,
    pub branch: String,
    pub base_sha: String,
    pub head_sha: String,
    pub status_entries: usize,
    pub changed_files: usize,
    pub untracked_files: usize,
    pub insertions: usize,
    pub deletions: usize,
}

/// One path in the review overlay: porcelain evidence plus the diff to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitFile {
    pub path: String,
    pub untracked: bool,
    pub insertions: usize,
    pub deletions: usize,
    pub diff: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitReview {
    pub facts: GitFacts,
    pub files: Vec<GitFile>,
    pub worktrees: Vec<(PathBuf, String)>,
}

/// Split a unified diff into per-path hunks. Untracked files are not in a diff.
pub fn split_diff_files(diff: &str) -> Vec<GitFile> {
    let mut files = Vec::new();
    let mut current_path = String::new();
    let mut current = String::new();
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if !current_path.is_empty() {
                files.push(GitFile {
                    path: std::mem::take(&mut current_path),
                    untracked: false,
                    insertions: current
                        .lines()
                        .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
                        .count(),
                    deletions: current
                        .lines()
                        .filter(|l| l.starts_with('-') && !l.starts_with("---"))
                        .count(),
                    diff: std::mem::take(&mut current),
                });
            }
            current_path = rest
                .split_once(" b/")
                .map(|(_, path)| path.to_owned())
                .or_else(|| rest.strip_prefix("a/").map(str::to_owned))
                .unwrap_or_else(|| rest.to_owned());
            current.push_str(line);
            current.push('\n');
        } else if !current_path.is_empty() {
            current.push_str(line);
            current.push('\n');
        }
    }
    if !current_path.is_empty() {
        files.push(GitFile {
            path: current_path,
            untracked: false,
            insertions: current
                .lines()
                .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
                .count(),
            deletions: current
                .lines()
                .filter(|l| l.starts_with('-') && !l.starts_with("---"))
                .count(),
            diff: current,
        });
    }
    files
}

#[derive(Debug, Clone)]
pub struct GitAdapter {
    worktree: PathBuf,
}

impl GitAdapter {
    pub fn new(worktree: impl Into<PathBuf>) -> Self {
        Self {
            worktree: worktree.into(),
        }
    }

    pub fn facts(&self, base: &str) -> Result<GitFacts, String> {
        // One `rev-parse` for three answers rather than three of them. Each `git` here is a fork,
        // an exec, and a repository discovery walk — around 13ms apiece on a warm cache — and
        // `rev-parse` was already being asked three separate times for facts it will happily
        // print together, in argument order, one per line.
        let revisions = self.git(["rev-parse", "--show-toplevel", "HEAD", base])?;
        let mut revisions = revisions.lines();
        let mut next = |what: &str| {
            revisions
                .next()
                .filter(|line| !line.trim().is_empty())
                .map(str::to_owned)
                .ok_or_else(|| format!("git rev-parse did not report {what}"))
        };
        let worktree = PathBuf::from(next("the worktree root")?);
        let worktree = std::fs::canonicalize(&worktree)
            .map_err(|error| format!("could not canonicalize live Git worktree: {error}"))?;
        let head_sha = next("HEAD")?;
        let base_sha = next("the base revision")?;
        let branch = self
            .git(["branch", "--show-current"])?
            .if_empty("DETACHED")
            .to_owned();
        let status = self.git(["status", "--porcelain=v1", "--untracked-files=normal"])?;
        // Untracked files are evidence, not a reason to refuse the handoff. Porcelain lines
        // (including `??`) are `status_entries`; `changed_files` stays the numstat against base.
        let (status_entries, untracked_files) = parse_porcelain(&status);
        let numstat = self.git(["diff", "--numstat", &base_sha])?;
        let (changed_files, insertions, deletions) = parse_numstat(&numstat);
        Ok(GitFacts {
            worktree,
            branch,
            base_sha,
            head_sha,
            status_entries,
            changed_files,
            untracked_files,
            insertions,
            deletions,
        })
    }

    /// The plain diff against `base`, with no external renderer.
    ///
    /// [`render_diff`](Self::render_diff) prefers `delta` for terminal output, but the dashboard
    /// overlay paints the diff with Dock's own palette — piping through `delta` first would only
    /// bury ANSI escapes inside text that then has to be un-escaped to be styled again.
    pub fn diff(&self, base: &str) -> Result<String, String> {
        let base_sha = self.git(["rev-parse", base])?;
        self.git(["diff", "--no-ext-diff", &base_sha])
    }

    /// Porcelain + numstat + per-file diffs + worktrees. Review only: no stage/commit/push.
    pub fn review(&self, base: &str) -> Result<GitReview, String> {
        let facts = self.facts(base)?;
        let porcelain = self.git(["status", "--porcelain=v1", "--untracked-files=normal"])?;
        let numstat = self.git(["diff", "--numstat", &facts.base_sha])?;
        let mut stats: std::collections::HashMap<String, (usize, usize)> =
            std::collections::HashMap::new();
        for line in numstat.lines() {
            let mut fields = line.splitn(3, '\t');
            let additions = fields.next().and_then(|v| v.parse().ok()).unwrap_or(0);
            let deletions = fields.next().and_then(|v| v.parse().ok()).unwrap_or(0);
            if let Some(path) = fields.next() {
                stats.insert(path.to_owned(), (additions, deletions));
            }
        }
        let mut files = Vec::new();
        for line in porcelain.lines().filter(|line| !line.is_empty()) {
            let untracked = line.starts_with("?? ");
            let path = if untracked {
                line[3..].to_owned()
            } else if line.len() >= 3 {
                line[3..].trim().to_owned()
            } else {
                continue;
            };
            let (insertions, deletions) = stats.get(&path).copied().unwrap_or((0, 0));
            let diff = if untracked {
                format!("untracked: {path}\n")
            } else {
                self.git(["diff", "--no-ext-diff", &facts.base_sha, "--", &path])
                    .unwrap_or_default()
            };
            files.push(GitFile {
                path,
                untracked,
                insertions,
                deletions,
                diff,
            });
        }
        let trees = worktrees(&self.worktree).unwrap_or_default();
        Ok(GitReview {
            facts,
            files,
            worktrees: trees,
        })
    }

    pub fn render_diff(&self, base: &str) -> Result<(String, bool), String> {
        let base_sha = self.git(["rev-parse", base])?;
        let raw = self.git(["diff", "--no-ext-diff", &base_sha])?;
        if raw.is_empty() {
            return Ok((raw, false));
        }
        let mut child = match Command::new("delta")
            .arg("--paging=never")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(_) => return Ok((raw, false)),
        };
        let Some(mut stdin) = child.stdin.take() else {
            return Ok((raw, false));
        };
        use std::io::Write;
        if stdin.write_all(raw.as_bytes()).is_err() {
            return Ok((raw, false));
        }
        drop(stdin);
        let output = child
            .wait_with_output()
            .map_err(|error| format!("delta did not complete: {error}"))?;
        if output.status.success() {
            return String::from_utf8(output.stdout)
                .map(|rendered| (rendered, true))
                .map_err(|error| format!("delta emitted non-UTF-8 output: {error}"));
        }
        Ok((raw, false))
    }

    fn git<const N: usize>(&self, args: [&str; N]) -> Result<String, String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.worktree)
            .args(args)
            .output()
            .map_err(|error| format!("failed to start git: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "git failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        String::from_utf8(output.stdout)
            .map(|stdout| stdout.trim().to_owned())
            .map_err(|error| format!("git emitted non-UTF-8 output: {error}"))
    }
}

trait EmptyFallback {
    fn if_empty(&self, fallback: &'static str) -> &str;
}

impl EmptyFallback for str {
    fn if_empty(&self, fallback: &'static str) -> &str {
        if self.is_empty() { fallback } else { self }
    }
}

fn parse_porcelain(status: &str) -> (usize, usize) {
    let mut entries = 0usize;
    let mut untracked = 0usize;
    for line in status.lines() {
        if line.is_empty() {
            continue;
        }
        entries += 1;
        if line.starts_with("?? ") {
            untracked += 1;
        }
    }
    (entries, untracked)
}

fn parse_numstat(numstat: &str) -> (usize, usize, usize) {
    numstat
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, '\t');
            let additions = fields.next()?;
            let deletions = fields.next()?;
            fields.next()?;
            Some((
                additions.parse::<usize>().unwrap_or(0),
                deletions.parse::<usize>().unwrap_or(0),
            ))
        })
        .fold(
            (0, 0, 0),
            |(files, additions, deletions), (added, deleted)| {
                (files + 1, additions + added, deletions + deleted)
            },
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numstat_counts_binary_rows_with_zero_line_counts() {
        let numstat = "12\t3\tsrc/main.rs\n-\t-\tassets/logo.png\n1\t0\tREADME.md\n";
        assert_eq!(parse_numstat(numstat), (3, 13, 3));
    }

    #[test]
    fn empty_numstat_means_clean_comparison() {
        assert_eq!(parse_numstat(""), (0, 0, 0));
    }

    #[test]
    fn split_diff_files_keeps_each_path() {
        let diff = concat!(
            "diff --git a/src/a.rs b/src/a.rs\n",
            "+one\n",
            "diff --git a/src/b.rs b/src/b.rs\n",
            "+two\n",
        );
        let files = split_diff_files(diff);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "src/a.rs");
        assert_eq!(files[1].path, "src/b.rs");
    }

    #[test]
    fn porcelain_counts_untracked_apart_from_the_numstat_diff() {
        assert_eq!(parse_porcelain(" M src/git.rs\n?? scratch.txt\n"), (2, 1));
        assert_eq!(parse_porcelain(""), (0, 0));
    }
}

#[cfg(test)]
mod worktree_tests {
    use super::*;
    use std::{
        fs,
        sync::atomic::{AtomicU32, Ordering},
    };

    static SEQUENCE: AtomicU32 = AtomicU32::new(0);

    struct Repo(PathBuf);

    impl Repo {
        /// Rooted outside the workspace: a fixture repository nested inside Dock's own would be
        /// governed by it, and `worktree add` would resolve against the wrong top-level.
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "dock-worktree-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            let repo = Self(fs::canonicalize(&root).unwrap());
            repo.git(["init", "-q", "-b", "main"]);
            repo.git(["config", "user.email", "dock@example.invalid"]);
            repo.git(["config", "user.name", "Dock Fixture"]);
            fs::write(repo.0.join("tracked"), "fixture\n").unwrap();
            repo.git(["add", "tracked"]);
            repo.git(["commit", "-qm", "fixture"]);
            repo
        }

        fn git<const N: usize>(&self, arguments: [&str; N]) -> String {
            run(&self.0, arguments).expect("git fixture command")
        }

        fn at(&self, name: &str) -> PathBuf {
            self.0.parent().unwrap().join(format!(
                "{}-{name}",
                self.0.file_name().unwrap().to_string_lossy()
            ))
        }
    }

    impl Drop for Repo {
        fn drop(&mut self) {
            for (path, _) in worktrees(&self.0).unwrap_or_default() {
                if path != self.0 {
                    let _ = fs::remove_dir_all(path);
                }
            }
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_new_task_gets_a_worktree_and_a_branch() {
        let repo = Repo::new();
        let path = repo.at("task-7");
        let worktree = ensure_worktree(&repo.0, "dock/task-7", &path, "HEAD").expect("create");
        assert!(worktree.created);
        assert_eq!(worktree.branch, "dock/task-7");
        assert!(
            worktree.path.join("tracked").exists(),
            "the worktree is checked out"
        );
        assert!(
            worktrees(&repo.0)
                .unwrap()
                .iter()
                .any(|(_, branch)| branch == "dock/task-7")
        );
    }

    #[test]
    fn dispatching_the_same_task_twice_lands_in_the_first_worktree() {
        let repo = Repo::new();
        let first = ensure_worktree(&repo.0, "dock/task-7", &repo.at("task-7"), "HEAD").unwrap();
        // A different path is suggested the second time; the branch already has a worktree, so
        // that one is used and nothing new is made.
        let second =
            ensure_worktree(&repo.0, "dock/task-7", &repo.at("task-7-again"), "HEAD").unwrap();
        assert!(
            !second.created,
            "the second dispatch must not create anything"
        );
        assert_eq!(first.path, second.path);
        assert!(!repo.at("task-7-again").exists());
    }

    #[test]
    fn an_existing_branch_is_checked_out_rather_than_recreated() {
        let repo = Repo::new();
        repo.git(["branch", "dock/existing"]);
        let worktree =
            ensure_worktree(&repo.0, "dock/existing", &repo.at("existing"), "HEAD").unwrap();
        assert!(worktree.created);
        assert_eq!(worktree.branch, "dock/existing");
    }

    #[test]
    fn an_occupied_path_is_refused_rather_than_written_into() {
        let repo = Repo::new();
        let path = repo.at("occupied");
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("someone-elses-work"), "keep me").unwrap();
        let refused = ensure_worktree(&repo.0, "dock/task-9", &path, "HEAD");
        assert!(refused.is_err(), "{refused:?}");
        // Whatever was there is still there: refusing is the whole point.
        assert!(path.join("someone-elses-work").exists());
        let _ = fs::remove_dir_all(&path);
    }

    #[test]
    fn a_worktree_needs_a_branch_name() {
        let repo = Repo::new();
        assert!(ensure_worktree(&repo.0, "   ", &repo.at("x"), "HEAD").is_err());
    }

    #[test]
    fn facts_still_report_the_worktree_head_and_base_after_one_combined_rev_parse() {
        // `facts` used to ask `rev-parse` three separate times — once for the worktree root, once
        // for HEAD, once for the base — at roughly 13ms per fork and exec. They are now one call
        // whose answers arrive in argument order, one per line, so this pins that order: reading
        // them back in the wrong order would silently swap a path into a SHA field.
        let repo = Repo::new();
        let facts = GitAdapter::new(&repo.0).facts("HEAD").expect("facts");
        assert_eq!(facts.worktree, repo.0);
        assert_eq!(facts.branch, "main");
        assert_eq!(facts.head_sha, facts.base_sha, "base HEAD resolves to HEAD");
        assert_eq!(facts.head_sha.len(), 40, "{}", facts.head_sha);
        assert!(facts.head_sha.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn untracked_files_are_counted_as_evidence_rather_than_refusing_facts() {
        let repo = Repo::new();
        fs::write(repo.0.join("scratch.txt"), "not tracked yet").unwrap();
        let facts = GitAdapter::new(&repo.0).facts("HEAD").expect("facts");
        assert_eq!(facts.status_entries, 1, "the untracked file is visible");
        assert_eq!(
            facts.changed_files, 0,
            "numstat against HEAD is still empty"
        );
    }

    #[test]
    fn facts_name_the_revision_that_could_not_be_resolved() {
        // A base that does not exist made the combined `rev-parse` fail rather than the single
        // one that used to ask for it, so the failure has to stay attributable.
        let repo = Repo::new();
        let refused = GitAdapter::new(&repo.0)
            .facts("no-such-base")
            .expect_err("an unknown base must be refused");
        assert!(refused.contains("git"), "{refused}");
    }

    #[test]
    fn the_listing_reports_the_main_worktree_and_its_branch() {
        let repo = Repo::new();
        let listed = worktrees(&repo.0).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].0, repo.0);
        assert_eq!(listed[0].1, "main");
    }
}
