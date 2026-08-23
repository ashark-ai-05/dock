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
    /// Added minimally by Task 7 so `protocol::DashboardProfile::Shell` compiles; Task 8 fills
    /// in its `resolve()` and `declared_capabilities()` behavior.
    Shell,
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
        // The override guard runs first so it applies uniformly to every built-in adapter,
        // including Shell below: a caller cannot pair `id: Shell` with `executable: Some(..)`
        // to smuggle an arbitrary binary past this check.
        if self.id != AdapterId::Generic && self.executable.is_some() {
            return Err("built-in adapter executables cannot be overridden; use generic for an explicit process".into());
        }
        if self.id == AdapterId::Shell {
            // The brief's snippet only falls back on a missing SHELL (env::var Err); the stated
            // requirement also covers SHELL being set but empty, so an empty value is treated
            // the same as absent here.
            let shell = env::var("SHELL")
                .ok()
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "/bin/sh".into());
            let executable = find_executable(&shell)
                .ok_or_else(|| format!("shell {shell:?} was not found or is not executable"))?;
            let mut command = vec![executable.display().to_string()];
            command.extend(self.arguments.clone());
            return Ok(ResolvedAdapter {
                id: AdapterId::Shell,
                executable,
                command,
                capabilities: AdapterCapabilities::NONE,
            });
        }
        let name = match self.id.default_executable() {
            Some(name) => name,
            None => self
                .executable
                .as_deref()
                .ok_or("generic adapter requires an explicit executable")?,
        };
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
            Self::Shell => None,
        }
    }

    /// How this agent is told to continue its most recent session, or `None` if Dock has no
    /// verified way to ask it.
    ///
    /// Every recipe here was read from the CLI's own `--help`, not assumed. That matters more than
    /// it looks: a wrong flag does not fail loudly, it starts a brand new session while the user
    /// believes they resumed one, and the work they were continuing is simply not there. An agent
    /// whose recipe has not been checked is therefore left as `None` and reported as unable to
    /// resume, which is a true statement about Dock rather than a guess about the agent.
    ///
    /// All three recipes resume *the most recent session for this working directory*, which is
    /// what makes them survive a daemon restart or a reboot: the agent stores the transcript
    /// itself and finds it again from where it is run. The cost of that convenience is ambiguity —
    /// two panes running the same agent in the same directory share a "most recent", so resuming
    /// one of them can land on the other's session.
    pub const fn resume_arguments(&self) -> Option<&'static [&'static str]> {
        match self {
            // `-c, --continue  Continue the most recent conversation in <cwd>`
            Self::ClaudeCode => Some(&["--continue"]),
            // `resume  Resume a previous interactive session (picker by default; --last to
            // continue the most recent)`
            Self::CodexCli => Some(&["resume", "--last"]),
            // `threads continue [threadId]  ... --last  Continue the last thread for the current
            // mode directly`
            Self::Amp => Some(&["threads", "continue", "--last"]),
            // Not installed anywhere its flags could be read, so nothing is claimed for it.
            Self::GithubCopilotCli => None,
            // A shell, a bare command, and the test fixture hold no session to continue.
            Self::Fixture | Self::Generic | Self::Shell => None,
        }
    }

    /// How this agent is handed an opening instruction, or nothing if Dock has no verified way.
    ///
    /// Read from each CLI's own `--help`, on the same principle as [`resume_arguments`]: a wrong
    /// guess here does not fail loudly, it launches the agent with the task text as a filename or
    /// a subcommand and leaves the person watching an error they did not cause. `amp` takes
    /// `[options] [command]` with no prompt positional at all, so it gets none — dispatching to it
    /// opens the agent in the right place, and [`Self::opening_prompt_is_typed`] carries the task
    /// the rest of the way.
    pub fn prompt_arguments(&self, prompt: &str) -> Vec<String> {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Vec::new();
        }
        match self {
            // `claude [options] [command] [prompt]`
            Self::ClaudeCode => vec![prompt.to_owned()],
            // `codex [OPTIONS] [PROMPT]`
            Self::CodexCli => vec![prompt.to_owned()],
            // `amp [options] [command]` — no prompt positional.
            Self::Amp => Vec::new(),
            // Not installed anywhere its arguments could be read.
            Self::GithubCopilotCli => Vec::new(),
            // The fixture runs a fixed script, and a shell handed a sentence would try to run it.
            Self::Fixture | Self::Generic | Self::Shell => Vec::new(),
        }
    }

    /// Whether an agent that took no prompt argument can be handed one by typing it instead.
    ///
    /// [`Self::prompt_arguments`] returns nothing for five adapters, but for two different reasons,
    /// and only one of them can be made good. Amp and Copilot are agents with an input box: they
    /// simply have nowhere on the command line to put a task, so typing it into the box once they
    /// are up says the same thing. The other three have no box to type into — a shell handed a
    /// sentence would try to *run* it, which is what `prompt_arguments` refuses them for, and
    /// typing it would be that same mistake one step later.
    pub const fn opening_prompt_is_typed(&self) -> bool {
        match self {
            Self::Amp | Self::GithubCopilotCli => true,
            // Both take the prompt on the command line, so there is nothing left to type.
            Self::ClaudeCode | Self::CodexCli => false,
            // No agent, no input box, and a sentence at a shell prompt is a command.
            Self::Fixture | Self::Generic | Self::Shell => false,
        }
    }

    /// The agent's name as a person would say it, for messages about what could not be done.
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Fixture => "the fixture",
            Self::Amp => "Amp",
            Self::ClaudeCode => "Claude Code",
            Self::CodexCli => "Codex CLI",
            Self::GithubCopilotCli => "GitHub Copilot CLI",
            Self::Generic => "this command",
            Self::Shell => "a shell",
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
            Self::Shell => AdapterCapabilities::NONE,
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

    #[test]
    fn shell_adapter_resolves_a_login_shell() {
        let resolved = AdapterSelection {
            id: AdapterId::Shell,
            executable: None,
            arguments: vec!["-l".into()],
        }
        .resolve()
        .expect("shell must resolve on any supported platform");
        assert_eq!(resolved.id, AdapterId::Shell);
        assert_eq!(resolved.command.last().map(String::as_str), Some("-l"));
    }

    #[test]
    fn shell_adapter_declares_no_provider_lifecycle() {
        assert!(!AdapterId::Shell.declared_capabilities().provider_lifecycle);
    }

    #[test]
    fn resume_recipes_match_what_each_cli_documents() {
        // Read from each CLI's own --help. A wrong flag would not fail loudly: it would start a
        // fresh session while the user believed they had continued one.
        assert_eq!(
            AdapterId::ClaudeCode.resume_arguments(),
            Some(&["--continue"][..])
        );
        assert_eq!(
            AdapterId::CodexCli.resume_arguments(),
            Some(&["resume", "--last"][..])
        );
        assert_eq!(
            AdapterId::Amp.resume_arguments(),
            Some(&["threads", "continue", "--last"][..])
        );
    }

    #[test]
    fn only_agents_that_document_a_prompt_positional_are_handed_one() {
        // Read from each CLI's own --help. Handing a sentence to something that does not take one
        // launches the agent with the task as a filename and blames the user for the error.
        assert_eq!(
            AdapterId::ClaudeCode.prompt_arguments("fix the retry path"),
            vec!["fix the retry path".to_owned()]
        );
        assert_eq!(
            AdapterId::CodexCli.prompt_arguments("fix the retry path"),
            vec!["fix the retry path".to_owned()]
        );
        // `amp [options] [command]` has no prompt positional.
        assert!(
            AdapterId::Amp
                .prompt_arguments("fix the retry path")
                .is_empty()
        );
        // A shell handed a sentence would try to run it; the fixture runs a fixed script.
        for adapter in [AdapterId::Shell, AdapterId::Fixture, AdapterId::Generic] {
            assert!(
                adapter.prompt_arguments("anything").is_empty(),
                "{adapter:?}"
            );
        }
        // An empty task title is never passed as an argument at all.
        assert!(AdapterId::ClaudeCode.prompt_arguments("   ").is_empty());
    }

    #[test]
    fn a_task_is_typed_only_to_an_agent_that_has_somewhere_to_type_it() {
        // The two that take no prompt argument but do have an input box: this is the gap that
        // made a dispatched card open a silent pane.
        for adapter in [AdapterId::Amp, AdapterId::GithubCopilotCli] {
            assert!(adapter.prompt_arguments("a task").is_empty(), "{adapter:?}");
            assert!(adapter.opening_prompt_is_typed(), "{adapter:?}");
        }
        // These take no prompt argument either, and typing one would be the mistake that
        // `prompt_arguments` refuses them for: at a shell prompt a sentence is a command.
        for adapter in [AdapterId::Shell, AdapterId::Generic, AdapterId::Fixture] {
            assert!(adapter.prompt_arguments("a task").is_empty(), "{adapter:?}");
            assert!(!adapter.opening_prompt_is_typed(), "{adapter:?}");
        }
        // And these already carried it on the command line, so there is nothing left to type.
        for adapter in [AdapterId::ClaudeCode, AdapterId::CodexCli] {
            assert!(
                !adapter.prompt_arguments("a task").is_empty(),
                "{adapter:?}"
            );
            assert!(!adapter.opening_prompt_is_typed(), "{adapter:?}");
        }
    }

    #[test]
    fn an_adapter_with_no_verified_recipe_claims_nothing() {
        // Nothing here holds a session to continue, or has had its flags checked. Claiming a
        // recipe on a guess is worse than admitting Dock cannot resume it.
        for adapter in [
            AdapterId::GithubCopilotCli,
            AdapterId::Fixture,
            AdapterId::Generic,
            AdapterId::Shell,
        ] {
            assert_eq!(adapter.resume_arguments(), None, "{adapter:?}");
        }
    }
}
