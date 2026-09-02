//! `.dock/checks.toml`, committed; and `~/.config/dock/checks.toml`, never committed.
//!
//! The repository declares what may run. The user declares which of their environment variables
//! a repository is allowed to see. Neither file may name the other's business, and an agent
//! writes to neither.

use std::{collections::BTreeMap, path::Path, time::Duration};

use serde::Deserialize;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

/// The parsed declaration file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checks {
    /// Whether checks run automatically at handoff. `r` from the receipt rail works either way.
    pub auto: bool,
    checks: BTreeMap<String, Check>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub run: Vec<String>,
    pub timeout: Duration,
    pub needs_env: Vec<String>,
}

/// What a name resolved to: something Dock may run, or a sentence saying why it may not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    Check(Check),
    Unwitnessed(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    checks: Option<Settings>,
    #[serde(default)]
    check: BTreeMap<String, Declaration>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    #[serde(default = "yes")]
    auto: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Declaration {
    /// An argv array. `serde` rejects a bare string here, which is the point: no `sh -c`, so no
    /// interpolation, no glob, no chaining. A pipeline is a committed script.
    run: Vec<String>,
    timeout: Option<String>,
    #[serde(default)]
    needs_env: Vec<String>,
}

const fn yes() -> bool {
    true
}

impl Checks {
    /// Reads `<repository>/.dock/checks.toml`. A repository with no file has no checks, which is
    /// not an error — it is the state that earns `no_checks_declared`.
    pub fn load(repository_root: &Path) -> Result<Self, String> {
        let path = repository_root.join(".dock").join("checks.toml");
        match std::fs::read_to_string(&path) {
            Ok(source) => Self::parse(&source),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                auto: true,
                checks: BTreeMap::new(),
            }),
            Err(error) => Err(format!("could not read {}: {error}", path.display())),
        }
    }

    pub fn parse(source: &str) -> Result<Self, String> {
        let file: File = toml::from_str(source)
            .map_err(|error| format!("could not read .dock/checks.toml: {error}"))?;
        let mut checks = BTreeMap::new();
        for (name, declaration) in file.check {
            if declaration.run.is_empty() {
                return Err(format!("check `{name}` declares an empty command"));
            }
            checks.insert(
                name.clone(),
                Check {
                    name,
                    timeout: parse_timeout(declaration.timeout.as_deref())?,
                    run: declaration.run,
                    needs_env: declaration.needs_env,
                },
            );
        }
        Ok(Self {
            auto: file.checks.is_none_or(|settings| settings.auto),
            checks,
        })
    }

    /// One map lookup. This is the containment argument: an agent supplies `name`, and the only
    /// thing a name can become is a value already in this map or a refusal.
    pub fn resolve(&self, name: &str, permitted: &[String]) -> Resolved {
        let Some(check) = self.checks.get(name) else {
            return Resolved::Unwitnessed(format!("no check named `{name}` in .dock/checks.toml"));
        };
        if let Some(missing) = check
            .needs_env
            .iter()
            .find(|name| !permitted.contains(name))
        {
            return Resolved::Unwitnessed(format!(
                "`{missing}` was requested by .dock/checks.toml and is not permitted in your user config."
            ));
        }
        Resolved::Check(check.clone())
    }
}

/// `~/.config/dock/checks.toml`'s `[permit] env`, following the same home convention
/// `dock_detect::manifest::override_dir` already uses. Absent file, empty list, no error.
pub fn load_permits() -> Result<Vec<String>, String> {
    let Some(home) = std::env::var_os("HOME") else {
        return Ok(Vec::new());
    };
    if home.is_empty() {
        return Ok(Vec::new());
    }
    let path = Path::new(&home)
        .join(".config")
        .join("dock")
        .join("checks.toml");
    let Ok(source) = std::fs::read_to_string(&path) else {
        return Ok(Vec::new());
    };
    let file: PermitFile = toml::from_str(&source)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    Ok(file.permit.map(|permit| permit.env).unwrap_or_default())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PermitFile {
    #[serde(default)]
    permit: Option<Permit>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Permit {
    #[serde(default)]
    env: Vec<String>,
}

fn parse_timeout(value: Option<&str>) -> Result<Duration, String> {
    let Some(value) = value else {
        return Ok(DEFAULT_TIMEOUT);
    };
    let (number, unit) = value.split_at(value.len().saturating_sub(1));
    let scale = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        _ => return Err(format!("timeout `{value}` must end in s, m or h")),
    };
    let seconds: u64 = number
        .parse()
        .map_err(|_| format!("timeout `{value}` is not a number"))?;
    if seconds == 0 {
        return Err(format!(
            "timeout `{value}` is zero, which would witness nothing"
        ));
    }
    Ok(Duration::from_secs(seconds * scale))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[checks]
auto = false

[check.test]
run     = ["cargo", "test", "--locked"]
timeout = "10m"

[check.publish]
run       = ["npm", "publish"]
needs_env = ["NPM_TOKEN"]
"#;

    #[test]
    fn a_declared_name_resolves_to_the_argv_the_repository_wrote() {
        let checks = Checks::parse(SAMPLE).expect("parse checks");
        assert!(!checks.auto);
        let Resolved::Check(check) = checks.resolve("test", &[]) else {
            panic!("`test` is declared and needs no environment");
        };
        assert_eq!(check.run, ["cargo", "test", "--locked"]);
        assert_eq!(check.timeout, Duration::from_secs(600));
    }

    /// The whole containment argument, in one assertion: a name nobody declared produces a
    /// refusal carrying the name, and there is nothing here that could become a command.
    #[test]
    fn an_undeclared_name_is_a_refusal_that_names_itself() {
        let checks = Checks::parse(SAMPLE).expect("parse checks");
        let Resolved::Unwitnessed(reason) = checks.resolve("typo", &[]) else {
            panic!("an undeclared name must never resolve to a command");
        };
        assert_eq!(reason, "no check named `typo` in .dock/checks.toml");
    }

    /// The repository may name a secret; only the user may permit it. An unpermitted request is
    /// refused in words rather than silently running without the variable.
    #[test]
    fn a_secret_the_user_has_not_permitted_is_refused_by_name() {
        let checks = Checks::parse(SAMPLE).expect("parse checks");
        let Resolved::Unwitnessed(reason) = checks.resolve("publish", &[]) else {
            panic!("an unpermitted variable must not run");
        };
        assert_eq!(
            reason,
            "`NPM_TOKEN` was requested by .dock/checks.toml and is not permitted in your user config."
        );
        assert!(matches!(
            checks.resolve("publish", &["NPM_TOKEN".to_owned()]),
            Resolved::Check(_)
        ));
    }

    /// A shell string is not an argv array, and the difference is the feature.
    #[test]
    fn a_run_that_is_not_an_argv_array_is_rejected_at_parse_time() {
        assert!(Checks::parse("[check.x]\nrun = \"cargo test && rm -rf /\"").is_err());
        assert!(Checks::parse("[check.x]\nrun = []").is_err());
    }

    /// Unknown keys are rejected rather than ignored: a declaration Dock half-understands is a
    /// declaration whose author believes something Dock is not doing.
    #[test]
    fn an_unknown_key_is_an_error_rather_than_a_shrug() {
        assert!(Checks::parse("[check.x]\nrun = [\"true\"]\nshell = true").is_err());
    }

    #[test]
    fn a_timeout_defaults_to_ten_minutes_and_understands_s_m_h() {
        assert_eq!(parse_timeout(None).unwrap(), Duration::from_secs(600));
        assert_eq!(parse_timeout(Some("90s")).unwrap(), Duration::from_secs(90));
        assert_eq!(parse_timeout(Some("5m")).unwrap(), Duration::from_secs(300));
        assert_eq!(
            parse_timeout(Some("1h")).unwrap(),
            Duration::from_secs(3600)
        );
        assert!(parse_timeout(Some("soon")).is_err());
        assert!(parse_timeout(Some("0m")).is_err());
    }
}
