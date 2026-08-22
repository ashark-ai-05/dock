use std::collections::{HashMap, HashSet, VecDeque};

use crate::detect::AgentKind;

/// Finds the agent executable running anywhere beneath one Dock-owned process-group leader.
///
/// The root is always a pid Dock's own `spawn` produced — the pane's process-group leader — so
/// this walk only ever inspects Dock's own descendants and can never become an adoption path
/// for arbitrary PIDs. What it deliberately does *not* do is test process-group equality: a
/// job-control shell puts every command it starts into a **new** process group, so the agent a
/// user launches by typing `claude` never shares the pane shell's pgid. Matching on pgid made
/// the product's own default workflow undetectable; parentage is the relation that survives it.
///
/// `table` is the output of `ps -axo pid=,ppid=,pgid=,comm=`.
pub fn agent_in_process_table(table: &str, leader_pid: i32) -> Option<AgentKind> {
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    let mut commands: HashMap<i32, &str> = HashMap::new();
    for line in table.lines() {
        let mut fields = line.split_whitespace();
        let Some(pid) = fields.next().and_then(|value| value.parse::<i32>().ok()) else {
            continue;
        };
        let Some(ppid) = fields.next().and_then(|value| value.parse::<i32>().ok()) else {
            continue;
        };
        // The pgid column is parsed only to keep the field positions honest; it is not a filter.
        if fields.next().is_none() {
            continue;
        }
        let Some(command) = fields.next() else {
            continue;
        };
        // A row cannot be its own parent. Recording one would build a self-loop that the visited
        // set would still absorb, but dropping it keeps the relation itself acyclic at the root.
        if ppid != pid {
            children.entry(ppid).or_default().push(pid);
        }
        commands.insert(pid, command);
    }
    // An explicit work list rather than recursion: a malformed or adversarial table must not be
    // able to drive stack depth, and `visited` makes any cycle terminate after one pass.
    let mut queue = VecDeque::from([leader_pid]);
    let mut visited = HashSet::new();
    while let Some(pid) = queue.pop_front() {
        if !visited.insert(pid) {
            continue;
        }
        if let Some(kind) = commands
            .get(&pid)
            .and_then(|command| executable_kind(command))
        {
            return Some(kind);
        }
        if let Some(descendants) = children.get(&pid) {
            queue.extend(descendants.iter().copied());
        }
    }
    None
}

fn executable_kind(command: &str) -> Option<AgentKind> {
    let executable = std::path::Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(command);
    AgentKind::from_executable(executable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_an_agent_that_is_the_leader_itself() {
        // The Dock-launched agent case: `Ctrl+B l` makes the agent its own group leader.
        let table = "\
  501   1  501 zsh
  902   1  902 codex
";
        assert_eq!(agent_in_process_table(table, 902), Some(AgentKind::Codex));
    }

    #[test]
    fn finds_an_agent_that_is_a_direct_child_of_the_leader() {
        let table = "\
  501   1  501 zsh
  777 501  777 /usr/local/bin/claude
";
        assert_eq!(agent_in_process_table(table, 501), Some(AgentKind::Claude));
    }

    #[test]
    fn finds_an_agent_running_as_a_grandchild_in_its_own_process_group() {
        // The real case: the pane's shell is the leader, the user types `claude`, and the shell
        // execs it through a wrapper. Every descendant sits in a process group of its own.
        let table = "\
  501   1  501 /bin/zsh
  640 501  640 /bin/bash
  777 640  640 /Users/someone/.local/bin/claude
  902   1  902 codex
";
        assert_eq!(agent_in_process_table(table, 501), Some(AgentKind::Claude));
    }

    #[test]
    fn never_matches_a_process_outside_the_leaders_own_descendant_tree() {
        // `codex` here belongs to an unrelated tree. Reporting it would mean Dock claiming a
        // process it did not launch, which is exactly the adoption the design forbids.
        let table = "\
  501   1  501 /bin/zsh
  640 501  640 /bin/bash
  902   1  902 codex
  903 902  902 claude
";
        assert_eq!(agent_in_process_table(table, 501), None);
    }

    #[test]
    fn reports_nothing_when_the_tree_holds_no_agent() {
        let table = "\
  501   1  501 /bin/zsh
  640 501  640 vim
  641 640  640 less
";
        assert_eq!(agent_in_process_table(table, 501), None);
        assert_eq!(agent_in_process_table(table, 4242), None);
    }

    #[test]
    fn ignores_malformed_rows_and_still_reads_the_valid_ones() {
        let table = "\
not a process row at all
  501   1  501 /bin/zsh
  abc  xyz  501 claude
  640
  641 640
  777 501  777 /usr/local/bin/claude
";
        assert_eq!(agent_in_process_table(table, 501), Some(AgentKind::Claude));
    }

    #[test]
    fn terminates_on_a_parent_cycle_instead_of_hanging_or_overflowing() {
        // `ps` cannot really produce this, but the walk must be total over its input.
        let table = "\
  501 640  501 /bin/zsh
  640 501  501 /bin/bash
  700 700  700 /bin/dash
";
        assert_eq!(agent_in_process_table(table, 501), None);
        assert_eq!(agent_in_process_table(table, 700), None);
    }
}
