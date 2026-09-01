//! Watch a board directory so the dashboard reloads when files change.
//!
//! Mtime on the next key or focus is how a TUI notices a board another process rewrote, but it
//! only runs when something else already woke the loop. A watcher on the directory fires because
//! the files changed, including when Dock itself is sitting idle on an open board.

use std::{
    path::{Path, PathBuf},
    sync::mpsc,
    time::Duration,
};

use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};

/// A live watch on one board directory, or a no-op if the host cannot watch files.
pub struct BoardWatcher {
    watcher: Option<RecommendedWatcher>,
    rx: mpsc::Receiver<()>,
    watching: Option<PathBuf>,
}

impl BoardWatcher {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        let watcher = RecommendedWatcher::new(
            move |result: Result<Event, notify::Error>| {
                if result.is_ok() {
                    let _ = tx.send(());
                }
            },
            Config::default().with_poll_interval(Duration::from_millis(200)),
        )
        .ok();
        Self {
            watcher,
            rx,
            watching: None,
        }
    }

    /// Point the watch at `directory`, replacing any previous target.
    pub fn ensure(&mut self, directory: &Path) {
        if self.watching.as_deref() == Some(directory) {
            return;
        }
        let Some(watcher) = self.watcher.as_mut() else {
            return;
        };
        if let Some(previous) = self.watching.take() {
            let _ = watcher.unwatch(&previous);
        }
        if watcher
            .watch(directory, RecursiveMode::NonRecursive)
            .is_ok()
        {
            self.watching = Some(directory.to_owned());
        }
    }

    /// True if at least one filesystem event arrived since the last call.
    pub fn take_changed(&mut self) -> bool {
        let mut changed = false;
        while self.rx.try_recv().is_ok() {
            changed = true;
        }
        changed
    }
}

impl Default for BoardWatcher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, thread, time::Instant};

    #[test]
    fn writing_a_task_file_wakes_the_watcher() {
        let dir = std::env::temp_dir().join(format!(
            "dock-board-watch-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let mut watcher = BoardWatcher::new();
        watcher.ensure(&dir);
        // The first event can be the watch itself; drain it.
        thread::sleep(Duration::from_millis(50));
        let _ = watcher.take_changed();
        fs::write(dir.join("1.md"), "---\nid: 1\nstatus: backlog\n---\n").unwrap();
        let start = Instant::now();
        let mut saw = false;
        while start.elapsed() < Duration::from_secs(3) {
            if watcher.take_changed() {
                saw = true;
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let _ = fs::remove_dir_all(&dir);
        assert!(
            saw,
            "a write to the watched directory must surface as take_changed"
        );
    }
}
