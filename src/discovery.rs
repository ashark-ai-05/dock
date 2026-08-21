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
