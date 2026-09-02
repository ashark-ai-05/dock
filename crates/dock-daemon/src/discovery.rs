use std::{path::Path, process::Command};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalAgentCandidate {
    pub provider: String,
    pub repository_match: bool,
}

impl ExternalAgentCandidate {
    pub fn status(&self) -> &'static str {
        "external/read-only"
    }
}

pub trait AgentDiscovery {
    fn discover(&self, repository: &Path) -> Vec<ExternalAgentCandidate>;
}

/// Not wired into the dashboard.
///
/// It was, and what it produced was a list of agent processes from the whole machine, scanned once
/// at startup and never refreshed — so it included agents running in Dock's own panes, in unrelated
/// terminals, and the user's own editor session, all under a heading promising "existing agents".
/// None of it was actionable either, because Dock has no adoption path by design. It is kept
/// because the intent is sound: knowing an agent is running outside Dock is worth showing, once it
/// is scoped to something meaningful (the `repository` argument below is still ignored) and
/// refreshed rather than frozen at launch.
pub struct ProcessNameDiscovery;

impl AgentDiscovery for ProcessNameDiscovery {
    fn discover(&self, _repository: &Path) -> Vec<ExternalAgentCandidate> {
        let Ok(output) = Command::new("ps").args(["-axo", "comm="]).output() else {
            return Vec::new();
        };
        let text = String::from_utf8_lossy(&output.stdout);
        let mut providers = Vec::new();
        for command in text.lines() {
            let executable = Path::new(command)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(command);
            let provider = match executable {
                "codex" => "Codex CLI",
                "claude" => "Claude Code",
                "amp" => "Amp",
                "github-copilot-cli" | "copilot" => "GitHub Copilot CLI",
                _ => continue,
            };
            if !providers
                .iter()
                .any(|item: &ExternalAgentCandidate| item.provider == provider)
            {
                providers.push(ExternalAgentCandidate {
                    provider: provider.into(),
                    repository_match: false,
                });
            }
        }
        providers
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_has_display_only_data_and_state() {
        let candidate = ExternalAgentCandidate {
            provider: "Codex CLI".into(),
            repository_match: true,
        };
        assert_eq!(candidate.status(), "external/read-only");
        let debug = format!("{candidate:?}");
        assert!(!debug.contains("pid"));
        assert!(!debug.contains("capacity"));
    }
}
