//! Which ambient environment variables a Dock-owned child may inherit.
//!
//! This started as a private detail of the PTY runtime, because a pane's child was the only
//! process Dock started. A declared check is the second, and it needs the same answer for the
//! same reason: whatever Dock spawns is reachable by an agent, so a credential in Dock's own
//! environment must not travel into it by accident. The policy is about environment variables
//! rather than about terminals, so it lives with the shapes rather than with the PTY.
//!
//! Only the question lives here. *Applying* the answer to a `std::process::Command` stays in the
//! crate that owns the child, because this crate denies `Command::new` and must keep denying it.

pub fn environment_is_allowed(key: &std::ffi::OsStr) -> bool {
    let key = key.to_string_lossy();
    matches!(
        key.as_ref(),
        // `TERM` is deliberately absent: `apply_child_environment` sets it to `PANE_TERM`,
        // because it describes the emulator the child is connected to rather than the terminal
        // Dock happens to be running inside. `COLORTERM` stays, because whether the *outer*
        // terminal can display 24-bit colour is a fact about the host that only the host knows.
        "COLORTERM" | "HOME" | "LANG" | "LOGNAME" | "PATH" | "SHELL" | "TMPDIR" | "USER"
    ) || key.starts_with("LC_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_environment_allowlist_excludes_credential_shaped_ambient_values() {
        for poisoned in [
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "AWS_SECRET_ACCESS_KEY",
            "GITHUB_TOKEN",
            "SSH_AUTH_SOCK",
            "CODEX_API_KEY",
        ] {
            assert!(!environment_is_allowed(std::ffi::OsStr::new(poisoned)));
        }
        for safe in ["HOME", "LANG", "LC_ALL", "PATH", "TMPDIR"] {
            assert!(environment_is_allowed(std::ffi::OsStr::new(safe)));
        }
    }
}
