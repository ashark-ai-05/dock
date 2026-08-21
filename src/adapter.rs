use std::{
    env, fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AdapterId {
    Fixture,
    Amp,
    ClaudeCode,
    CodexCli,
    GithubCopilotCli,
    Generic,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdapterSelection {
    pub id: AdapterId,
    #[serde(default)]
    pub executable: Option<String>,
    #[serde(default)]
    pub arguments: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdapterCapabilities {
    /// Provider-native operations only. Dock-owned process controls are reported separately.
    pub attach: bool,
    pub focus: bool,
    pub interrupt: bool,
    pub stop: bool,
    pub restart: bool,
    /// Generic commands expose process facts only; Dock never infers provider semantics.
    pub provider_lifecycle: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessCapabilities {
    pub attach: bool,
    pub focus: bool,
    pub interrupt: bool,
    pub stop: bool,
    pub restart: bool,
}

impl ProcessCapabilities {
    pub const OWNED_RUNTIME: Self = Self {
        attach: true,
        focus: true,
        interrupt: true,
        stop: true,
        restart: true,
    };
}

#[derive(Debug, Clone)]
pub struct ResolvedAdapter {
    pub id: AdapterId,
    pub executable: PathBuf,
    pub command: Vec<String>,
    pub capabilities: AdapterCapabilities,
}

impl AdapterSelection {
    pub fn resolve(&self) -> Result<ResolvedAdapter, String> {
        let name = match self.id.default_executable() {
            Some(name) => name,
            None => self
                .executable
                .as_deref()
                .ok_or("generic adapter requires an explicit executable")?,
        };
        if self.id != AdapterId::Generic && self.executable.is_some() {
            return Err("built-in adapter executables cannot be overridden; use generic for an explicit process".into());
        }
        if name.trim().is_empty() {
            return Err("adapter executable cannot be empty".into());
        }
        let executable = find_executable(name).ok_or_else(|| format!(
            "adapter {:?} requires executable {name:?}, but it was not found or executable; install it or select a configured generic adapter",
            self.id
        ))?;
        let mut command = vec![executable.display().to_string()];
        command.extend(self.arguments.clone());
        Ok(ResolvedAdapter {
            id: self.id.clone(),
            executable,
            command,
            capabilities: self.id.declared_capabilities(),
        })
    }
}

impl AdapterId {
    pub const fn default_executable(&self) -> Option<&'static str> {
        match self {
            Self::Fixture => Some("sh"),
            Self::Amp => Some("amp"),
            Self::ClaudeCode => Some("claude"),
            Self::CodexCli => Some("codex"),
            Self::GithubCopilotCli => Some("copilot"),
            Self::Generic => None,
        }
    }

    /// Provider-native claims are enumerated per profile. None of the current CLIs has a verified
    /// provider lifecycle contract; Dock-owned process controls are declared separately.
    pub const fn declared_capabilities(&self) -> AdapterCapabilities {
        match self {
            Self::Fixture => AdapterCapabilities::NONE,
            Self::Amp => AdapterCapabilities::NONE,
            Self::ClaudeCode => AdapterCapabilities::NONE,
            Self::CodexCli => AdapterCapabilities::NONE,
            Self::GithubCopilotCli => AdapterCapabilities::NONE,
            Self::Generic => AdapterCapabilities::NONE,
        }
    }
}

impl AdapterCapabilities {
    pub const NONE: Self = Self {
        attach: false,
        focus: false,
        interrupt: false,
        stop: false,
        restart: false,
        provider_lifecycle: false,
    };
}

fn find_executable(name: &str) -> Option<PathBuf> {
    let candidate = Path::new(name);
    if candidate.components().count() > 1 {
        return executable(candidate)
            .then(|| fs::canonicalize(candidate).ok())
            .flatten();
    }
    env::var_os("PATH")?
        .as_os_str()
        .to_str()?
        .split(':')
        .filter(|part| !part.is_empty())
        .map(|part| Path::new(part).join(name))
        .find(|path| executable(path))
        .and_then(|path| fs::canonicalize(path).ok())
}

fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

pub fn builtin_available(id: &AdapterId) -> bool {
    id.default_executable()
        .is_some_and(|name| find_executable(name).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn profiles_have_stable_binary_names() {
        let cases = [
            (AdapterId::Amp, "amp"),
            (AdapterId::ClaudeCode, "claude"),
            (AdapterId::CodexCli, "codex"),
            (AdapterId::GithubCopilotCli, "copilot"),
        ];
        for (id, expected) in cases {
            assert_eq!(id.default_executable(), Some(expected));
            let selection = AdapterSelection {
                id,
                executable: None,
                arguments: vec![],
            };
            match selection.resolve() {
                Ok(found) => assert!(!found.command.is_empty()),
                Err(error) => assert!(error.contains(expected)),
            }
        }
    }
    #[test]
    fn fixture_is_deterministic_and_generic_requires_explicit_binary() {
        assert_eq!(
            AdapterSelection {
                id: AdapterId::Fixture,
                executable: None,
                arguments: vec!["-c".into(), "exit 0".into()]
            }
            .resolve()
            .unwrap()
            .id,
            AdapterId::Fixture
        );
        assert!(
            AdapterSelection {
                id: AdapterId::Generic,
                executable: None,
                arguments: vec![]
            }
            .resolve()
            .is_err()
        );
    }

    #[test]
    fn every_adapter_defaults_provider_capabilities_to_absent() {
        for id in [
            AdapterId::Fixture,
            AdapterId::Amp,
            AdapterId::ClaudeCode,
            AdapterId::CodexCli,
            AdapterId::GithubCopilotCli,
            AdapterId::Generic,
        ] {
            assert_eq!(id.declared_capabilities(), AdapterCapabilities::default());
        }
    }
}
