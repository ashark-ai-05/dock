//! Listing the files under a directory, for the file picker.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

/// The most paths a listing will return.
///
/// A picker the user filters by typing does not become more useful past a few thousand rows, and a
/// monorepo can hold hundreds of thousands. The cap keeps one keystroke from walking all of them.
pub const LISTING_LIMIT: usize = 5_000;

/// Paths under `root`, relative to it, `/`-separated and sorted.
///
/// Git is asked first, because in a repository it already knows the answer: it lists tracked and
/// untracked files while honouring every `.gitignore`, which is what stops `target/` and
/// `node_modules/` from burying the files a person actually wants. Outside a repository, or when
/// Git is unavailable, this falls back to walking the tree with the same directories skipped by
/// name — the best approximation available without a gitignore parser.
///
/// Best-effort throughout: an unreadable directory contributes nothing rather than failing the
/// listing, since a picker that shows most of the tree beats one that shows an error.
pub fn list(root: &Path, limit: usize) -> Vec<String> {
    let mut paths = git_listing(root, limit).unwrap_or_else(|| walk(root, limit));
    paths.sort();
    paths.truncate(limit);
    paths
}

/// Directories never worth walking into: version-control internals and the conventional build and
/// dependency caches. Only consulted on the non-Git path, where there is no `.gitignore` to obey.
const SKIPPED: [&str; 6] = [".git", "target", "node_modules", ".venv", "dist", "build"];

fn git_listing(root: &Path, limit: usize) -> Option<Vec<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .split('\0')
            .filter(|path| !path.is_empty())
            .take(limit)
            .map(str::to_owned)
            .collect(),
    )
}

fn walk(root: &Path, limit: usize) -> Vec<String> {
    let mut found = Vec::new();
    let mut pending = vec![PathBuf::from(root)];
    while let Some(directory) = pending.pop() {
        if found.len() >= limit {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            // A symlinked directory can point back up the tree, so only real directories are
            // descended into; a symlink to a file is still offered as a file.
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if !name.starts_with('.') && !SKIPPED.contains(&name.as_str()) {
                    pending.push(entry.path());
                }
                continue;
            }
            if name.starts_with('.') {
                continue;
            }
            if let Ok(relative) = entry.path().strip_prefix(root) {
                found.push(relative.to_string_lossy().replace('\\', "/"));
            }
            if found.len() >= limit {
                break;
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::atomic::{AtomicU32, Ordering},
    };

    static SEQUENCE: AtomicU32 = AtomicU32::new(0);

    struct Tree(PathBuf);

    impl Tree {
        /// Rooted in the system temp directory rather than under `target/`, which sits inside
        /// Dock's own repository: a fixture there is governed by Dock's `.gitignore`, so the Git
        /// path would answer for it and the plain walk these tests exercise would never run.
        fn new(label: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "dock-files-{label}-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn file(&self, relative: &str) -> &Self {
            let path = self.0.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "x").unwrap();
            self
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_plain_directory_is_walked_and_returned_sorted_and_relative() {
        let tree = Tree::new("walk");
        tree.file("b.txt").file("a.txt").file("src/main.rs");
        assert_eq!(
            list(&tree.0, LISTING_LIMIT),
            ["a.txt", "b.txt", "src/main.rs"]
        );
    }

    #[test]
    fn build_output_and_dotfiles_are_skipped_outside_a_repository() {
        let tree = Tree::new("skips");
        tree.file("keep.rs")
            .file("target/debug/huge.bin")
            .file("node_modules/pkg/index.js")
            .file(".secret")
            .file(".hidden/inside.txt");
        assert_eq!(list(&tree.0, LISTING_LIMIT), ["keep.rs"]);
    }

    #[test]
    fn the_limit_is_honoured_so_one_keystroke_cannot_walk_a_monorepo() {
        let tree = Tree::new("limit");
        for index in 0..20 {
            tree.file(&format!("file-{index:02}.txt"));
        }
        let listed = list(&tree.0, 5);
        assert_eq!(listed.len(), 5);
    }

    #[test]
    fn a_missing_directory_lists_nothing_rather_than_failing() {
        assert!(list(Path::new("/nonexistent-dock-fixture"), LISTING_LIMIT).is_empty());
    }

    #[test]
    fn a_repository_listing_honours_gitignore_where_a_plain_walk_cannot() {
        let tree = Tree::new("git");
        tree.file("kept.rs").file("ignored.log").file(".gitignore");
        fs::write(tree.0.join(".gitignore"), "*.log\n").unwrap();
        let git = |arguments: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(&tree.0)
                .args(arguments)
                .output()
                .expect("run git")
        };
        git(&["init", "-q"]);
        let listed = list(&tree.0, LISTING_LIMIT);
        assert!(listed.contains(&"kept.rs".to_owned()), "{listed:?}");
        assert!(
            !listed.contains(&"ignored.log".to_owned()),
            "an ignored file must not be offered: {listed:?}"
        );
        // The plain walk cannot know about .gitignore, which is exactly why Git is asked first.
        assert!(walk(&tree.0, LISTING_LIMIT).contains(&"ignored.log".to_owned()));
    }
}
