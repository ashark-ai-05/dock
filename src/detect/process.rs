use crate::detect::AgentKind;

/// Finds the agent executable running inside one Dock-owned process group.
///
/// Scoping to the pane's own PGID is what keeps this honest: Dock only ever classifies
/// processes it launched, so this can never become an adoption path for arbitrary PIDs.
/// `table` is the output of `ps -axo pid=,ppid=,pgid=,comm=`.
pub fn agent_in_process_table(table: &str, pgid: i32) -> Option<AgentKind> {
    table
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let _pid = fields.next()?;
            let _ppid = fields.next()?;
            let row_pgid: i32 = fields.next()?.parse().ok()?;
            let command = fields.next()?;
            (row_pgid == pgid).then_some(command)
        })
        .filter_map(|command| {
            let executable = std::path::Path::new(command)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(command);
            AgentKind::from_executable(executable)
        })
        .next()
}
