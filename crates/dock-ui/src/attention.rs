//! Ranked jump between agents that need a person.

use dock_detect::AgentState;

/// One live pane considered for [`rank_attention`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttentionCandidate<'a> {
    pub workspace_id: &'a str,
    pub pane_id: &'a str,
    pub state: AgentState,
    /// Only recognised agents enter the cycle. A shell is never "needing you".
    pub is_agent: bool,
    /// A done pane already looked at this turn: skip until it leaves done.
    pub seen: bool,
}

/// Rank agents that need you: blocked first, then unseen done, then idle last
/// when `include_idle` is set. Working is never in the cycle. Workspace-local
/// panes outrank the same bucket in another workspace.
pub fn rank_attention<'a>(
    current_workspace: &str,
    include_idle: bool,
    candidates: impl IntoIterator<Item = AttentionCandidate<'a>>,
) -> Vec<(String, String)> {
    let mut items: Vec<AttentionCandidate<'a>> = candidates
        .into_iter()
        .filter(|candidate| candidate.is_agent)
        .filter(|candidate| match candidate.state {
            AgentState::Working => false,
            AgentState::Idle => include_idle,
            AgentState::Blocked => true,
            AgentState::Done => !candidate.seen,
        })
        .collect();
    items.sort_by_key(|candidate| {
        let bucket = match candidate.state {
            AgentState::Blocked => 0u8,
            AgentState::Done => 1,
            AgentState::Idle => 2,
            AgentState::Working => 3,
        };
        let locality = u8::from(candidate.workspace_id != current_workspace);
        (bucket, locality)
    });
    items
        .into_iter()
        .map(|candidate| {
            (
                candidate.workspace_id.to_owned(),
                candidate.pane_id.to_owned(),
            )
        })
        .collect()
}

/// Next pane in the cycle after `current`, wrapping. Empty list means nobody.
/// Worst agent state in a workspace: blocked beats working beats done beats idle.
pub fn worst_state(states: impl IntoIterator<Item = AgentState>) -> Option<AgentState> {
    states
        .into_iter()
        .min_by_key(|state| state.attention_rank())
}

pub fn next_attention(
    ranked: &[(String, String)],
    current: Option<(&str, &str)>,
) -> Option<(String, String)> {
    if ranked.is_empty() {
        return None;
    }
    let Some((workspace, pane)) = current else {
        return ranked.first().cloned();
    };
    match ranked
        .iter()
        .position(|(here, there)| here == workspace && there == pane)
    {
        Some(index) => ranked
            .get(index + 1)
            .cloned()
            .or_else(|| ranked.first().cloned()),
        None => ranked.first().cloned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand<'a>(
        workspace: &'a str,
        pane: &'a str,
        state: AgentState,
        seen: bool,
    ) -> AttentionCandidate<'a> {
        AttentionCandidate {
            workspace_id: workspace,
            pane_id: pane,
            state,
            is_agent: true,
            seen,
        }
    }

    #[test]
    fn blocked_beats_unseen_done_and_working_is_skipped() {
        let ranked = rank_attention(
            "here",
            false,
            [
                cand("here", "working", AgentState::Working, false),
                cand("here", "done", AgentState::Done, false),
                cand("here", "blocked", AgentState::Blocked, false),
                cand("here", "idle", AgentState::Idle, false),
            ],
        );
        assert_eq!(
            ranked,
            vec![
                ("here".into(), "blocked".into()),
                ("here".into(), "done".into()),
            ]
        );
    }

    #[test]
    fn seen_done_is_skipped_and_idle_is_last_only_when_asked() {
        let without_idle = rank_attention(
            "here",
            false,
            [
                cand("here", "seen", AgentState::Done, true),
                cand("here", "idle", AgentState::Idle, false),
            ],
        );
        assert!(without_idle.is_empty());
        let with_idle = rank_attention(
            "here",
            true,
            [
                cand("here", "seen", AgentState::Done, true),
                cand("here", "idle", AgentState::Idle, false),
                cand("here", "blocked", AgentState::Blocked, false),
            ],
        );
        assert_eq!(
            with_idle,
            vec![
                ("here".into(), "blocked".into()),
                ("here".into(), "idle".into()),
            ]
        );
    }

    #[test]
    fn local_workspace_outranks_the_same_bucket_elsewhere() {
        let ranked = rank_attention(
            "here",
            false,
            [
                cand("there", "blocked", AgentState::Blocked, false),
                cand("here", "blocked", AgentState::Blocked, false),
                cand("there", "done", AgentState::Done, false),
                cand("here", "done", AgentState::Done, false),
            ],
        );
        assert_eq!(
            ranked,
            vec![
                ("here".into(), "blocked".into()),
                ("there".into(), "blocked".into()),
                ("here".into(), "done".into()),
                ("there".into(), "done".into()),
            ]
        );
    }

    #[test]
    fn a_shell_never_enters_the_cycle() {
        let ranked = rank_attention(
            "here",
            true,
            [AttentionCandidate {
                workspace_id: "here",
                pane_id: "shell",
                state: AgentState::Blocked,
                is_agent: false,
                seen: false,
            }],
        );
        assert!(ranked.is_empty());
    }

    #[test]
    fn worst_state_is_blocked_then_working() {
        assert_eq!(
            worst_state([AgentState::Idle, AgentState::Working, AgentState::Blocked]),
            Some(AgentState::Blocked)
        );
        assert_eq!(worst_state(Vec::<AgentState>::new()), None);
    }

    #[test]
    fn next_wraps_and_nobody_is_none() {
        let ranked = vec![("here".into(), "a".into()), ("here".into(), "b".into())];
        assert_eq!(
            next_attention(&ranked, Some(("here", "a"))),
            Some(("here".into(), "b".into()))
        );
        assert_eq!(
            next_attention(&ranked, Some(("here", "b"))),
            Some(("here".into(), "a".into()))
        );
        assert_eq!(
            next_attention(&ranked, Some(("here", "elsewhere"))),
            Some(("here".into(), "a".into()))
        );
        assert_eq!(next_attention(&[], Some(("here", "a"))), None);
    }
}
