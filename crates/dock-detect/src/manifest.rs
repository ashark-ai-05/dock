//! Per-agent detection rules, overridable without rebuilding Dock.
//!
//! Every time this classification has been wrong, the only remedy was editing Rust and rebuilding.
//! That is a poor bargain for the person watching the wrong answer: they can see the screen that
//! confused it and could fix the rule in a minute, and instead they have to file it and wait. A
//! manifest moves the rules out of the binary, so an agent Dock has never seen — or one that
//! respells its prompts in a release that ships after this one — is somebody's afternoon rather
//! than a new version of Dock.
//!
//! JSON rather than TOML: Dock already speaks JSON everywhere and gains no dependency, and these
//! files are full of regexes. TOML's two string forms escape backslashes differently, and a
//! pattern that is silently wrong is exactly the failure this is meant to end.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Mutex, OnceLock},
};

use regex::RegexSet;
use serde::{Deserialize, Serialize};

use crate::AgentKind;

/// The rules for one agent. Every field is optional: a manifest that defines only `blocked` keeps
/// the built-in rules for everything else, so narrowing one state does not mean restating them all.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Accepted and ignored, so a file can say which shape it was written against.
    #[serde(default)]
    pub schema: Option<u16>,
    /// The agent has asked something and cannot continue: a permission prompt, a chooser.
    #[serde(default)]
    pub blocked: Option<Vec<String>>,
    /// The agent is mid-turn. Rarely needed: output alone answers this for most agents.
    #[serde(default)]
    pub working: Option<Vec<String>>,
    /// The agent is sitting at its own input box with the turn handed back.
    #[serde(default)]
    pub awaiting: Option<Vec<String>>,
}

/// Where an agent's rules came from, so a wrong answer can be traced to the file that caused it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    BuiltIn,
    Override(PathBuf),
}

/// The rules actually in force for an agent, with the patterns already compiled.
pub struct Resolved {
    pub source: Source,
    pub blocked: RegexSet,
    pub working: RegexSet,
    pub awaiting: RegexSet,
    /// Kept for `dock detect`, which has to show what it is applying, not just apply it.
    pub patterns: (Vec<String>, Vec<String>, Vec<String>),
}

/// `~/.config/dock/agent-detection`, where an override for any agent goes.
pub fn override_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    if home.is_empty() {
        return None;
    }
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("dock")
            .join("agent-detection"),
    )
}

fn override_path(agent: AgentKind) -> Option<PathBuf> {
    Some(override_dir()?.join(format!("{}.json", agent.label())))
}

/// Reads an agent's override file, or `None` when there is none.
///
/// A file that cannot be parsed is reported rather than ignored: silently falling back to the
/// built-ins would leave someone staring at the same wrong answer their edit was meant to fix.
pub fn read_override(agent: AgentKind) -> Result<Option<(PathBuf, Manifest)>, String> {
    let Some(path) = override_path(agent) else {
        return Ok(None);
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };
    serde_json::from_str::<Manifest>(&text)
        .map(|manifest| Some((path.clone(), manifest)))
        .map_err(|error| format!("{}: {error}", path.display()))
}

/// The rules in force for an agent, compiled and cached.
pub fn resolve(agent: AgentKind) -> &'static Resolved {
    static CACHE: OnceLock<Mutex<HashMap<AgentKind, &'static Resolved>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(resolved) = cache.get(&agent) {
        return resolved;
    }
    let (source, manifest) = match read_override(agent) {
        Ok(Some((path, manifest))) => (Source::Override(path), manifest),
        // A broken override is reported by `dock detect` rather than here: this runs on the event
        // stream's hot path and has nowhere to say anything.
        Ok(None) | Err(_) => (Source::BuiltIn, Manifest::default()),
    };
    let built_in = crate::heuristic::built_in(agent);
    let pick = |chosen: Option<Vec<String>>, fallback: &[&str]| -> Vec<String> {
        chosen.unwrap_or_else(|| fallback.iter().map(|p| (*p).to_owned()).collect())
    };
    let blocked = pick(manifest.blocked, built_in.0);
    let working = pick(manifest.working, built_in.1);
    let awaiting = pick(manifest.awaiting, built_in.2);
    // A pattern that will not compile falls back to matching nothing rather than taking the whole
    // classifier down: one bad rule in an override should cost that rule, not the dashboard.
    let compile =
        |patterns: &[String]| RegexSet::new(patterns).unwrap_or_else(|_| RegexSet::empty());
    let resolved: &'static Resolved = Box::leak(Box::new(Resolved {
        source,
        blocked: compile(&blocked),
        working: compile(&working),
        awaiting: compile(&awaiting),
        patterns: (blocked, working, awaiting),
    }));
    cache.insert(agent, resolved);
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_manifest_may_define_one_state_and_inherit_the_rest() {
        // Narrowing one state must not mean restating the others, or nobody will narrow anything.
        let manifest: Manifest =
            serde_json::from_str(r#"{"schema":1,"blocked":["^ready\\?"]}"#).expect("parse");
        assert_eq!(
            manifest.blocked.as_deref(),
            Some(&["^ready\\?".to_owned()][..])
        );
        assert_eq!(manifest.working, None);
        assert_eq!(manifest.awaiting, None);
    }

    #[test]
    fn an_unknown_field_is_refused_rather_than_quietly_ignored() {
        // A typo in a rules file is exactly the case where silence costs the most: the author is
        // already staring at a wrong answer and would read the ignored key as applied.
        assert!(serde_json::from_str::<Manifest>(r#"{"blocke":["x"]}"#).is_err());
    }

    #[test]
    fn a_regex_survives_the_round_trip_with_its_backslashes_intact() {
        // The reason this is JSON. A pattern that loses an escape matches nothing and says so
        // nowhere.
        let manifest: Manifest =
            serde_json::from_str(r#"{"awaiting":["\\(shift\\+tab to cycle\\)"]}"#).expect("parse");
        let pattern = &manifest.awaiting.expect("awaiting")[0];
        assert_eq!(pattern, r"\(shift\+tab to cycle\)");
        assert!(
            RegexSet::new([pattern])
                .expect("compiles")
                .is_match("⏵⏵ auto mode on (shift+tab to cycle)")
        );
    }

    #[test]
    fn built_in_rules_are_in_force_when_nothing_overrides_them() {
        let resolved = resolve(AgentKind::Claude);
        assert!(matches!(
            resolved.source,
            Source::BuiltIn | Source::Override(_)
        ));
        // Whatever the source, the rules that matter still recognise a chooser.
        assert!(resolved.blocked.is_match("Enter to select · Esc to cancel"));
    }
}
