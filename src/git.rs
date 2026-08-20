use std::{
    path::PathBuf,
    process::{Command, Stdio},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitFacts {
    pub worktree: PathBuf,
    pub branch: String,
    pub base_sha: String,
    pub head_sha: String,
    pub status_entries: usize,
    pub changed_files: usize,
    pub insertions: usize,
    pub deletions: usize,
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
        let worktree = PathBuf::from(self.git(["rev-parse", "--show-toplevel"])?);
        let worktree = std::fs::canonicalize(&worktree)
            .map_err(|error| format!("could not canonicalize live Git worktree: {error}"))?;
        let head_sha = self.git(["rev-parse", "HEAD"])?;
        let branch = self
            .git(["branch", "--show-current"])?
            .if_empty("DETACHED")
            .to_owned();
        let base_sha = self.git(["rev-parse", base])?;
        let status = self.git(["status", "--porcelain=v1", "--untracked-files=normal"])?;
        if status.lines().any(|line| line.starts_with("?? ")) {
            return Err(
                "handoff Git evidence does not accept untracked files; track or remove them first"
                    .into(),
            );
        }
        let numstat = self.git(["diff", "--numstat", &base_sha])?;
        let (changed_files, insertions, deletions) = parse_numstat(&numstat);
        Ok(GitFacts {
            worktree,
            branch,
            base_sha,
            head_sha,
            status_entries: changed_files,
            changed_files,
            insertions,
            deletions,
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
}
