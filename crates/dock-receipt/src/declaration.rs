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
    load_permits_from(&path)
}

/// The part of `load_permits` that touches a concrete path, split out so a test can point it at
/// a path it controls instead of the real `~/.config/dock/checks.toml`.
///
/// Mirrors `Checks::load`: a missing file is the ordinary "nothing permitted yet" state, not an
/// error, but any other read failure — permission denied, the path being a directory, and so on
/// — is reported by name rather than folded into the same "not permitted" outcome a missing file
/// produces. Conflating the two would tell someone their config denies a variable when the truth
/// is Dock could not read the config at all.
fn load_permits_from(path: &Path) -> Result<Vec<String>, String> {
    let source = match std::fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("could not read {}: {error}", path.display())),
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
    // `u64::MAX` parses, and `u64::MAX * 3_600` panics in debug and wraps in release — to a
    // timeout of a few seconds, which would kill checks a repository asked to run for hours.
    let seconds = seconds
        .checked_mul(scale)
        .ok_or_else(|| format!("timeout `{value}` is longer than Dock can represent"))?;
    Ok(Duration::from_secs(seconds))
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

    /// A number that parses and then does not fit. `u64::MAX * 3_600` panicked in debug and, in
    /// release, wrapped to a handful of seconds — a repository asking for the longest timeout it
    /// could write would have got the shortest one there is, and its checks killed mid-run.
    #[test]
    fn a_timeout_too_large_to_represent_is_refused_rather_than_wrapped() {
        let huge = format!("{}h", u64::MAX);
        let refused = parse_timeout(Some(&huge)).expect_err("an unrepresentable timeout");
        assert!(
            refused.contains("longer than Dock can represent"),
            "{refused}"
        );
        // The largest one that does fit still parses, so the guard rejects only the impossible.
        let largest = format!("{}s", u64::MAX);
        assert_eq!(
            parse_timeout(Some(&largest)).unwrap(),
            Duration::from_secs(u64::MAX)
        );
    }

    /// A permission file that cannot be *read* — here, because the path is a directory rather
    /// than a file — must be reported, not folded into "the user permitted nothing." That
    /// conflation would send someone to edit a config that may already say what they intended;
    /// the real fault is that Dock could not read it at all.
    #[test]
    fn an_unreadable_permit_file_is_reported_rather_than_treated_as_no_permits() {
        let directory = std::env::temp_dir().join(format!(
            "dock-receipt-test-permit-dir-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&directory).expect("create directory fixture");

        let error = load_permits_from(&directory).expect_err("a directory is not a readable file");
        assert!(
            error.contains(&directory.display().to_string()),
            "error should name the unreadable path, got: {error}"
        );

        std::fs::remove_dir(&directory).expect("clean up directory fixture");
    }
}
