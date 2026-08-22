//! Wall-clock budgets for tests that wait on real subprocesses, PTYs, and sockets.

use std::{
    env,
    sync::OnceLock,
    time::{Duration, Instant},
};

/// The multiplier applied to every budget, read once from `DOCK_TEST_TIMEOUT_SCALE`.
///
/// A missing, unparseable, or zero value means 1, so an unset environment behaves exactly as the
/// suite did before this knob existed.
fn scale() -> u64 {
    static SCALE: OnceLock<u64> = OnceLock::new();
    *SCALE.get_or_init(|| {
        env::var("DOCK_TEST_TIMEOUT_SCALE")
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(1)
    })
}

/// How long a test is willing to wait before it calls a behaviour absent.
///
/// These budgets are liveness backstops: they exist so a genuine regression fails with a message
/// instead of hanging forever, not to assert how fast Dock is. Growing one changes nothing about
/// what a test asserts — only how much patience it extends before concluding the behaviour never
/// arrived. A shared CI runner is slower and far more contended than a developer machine and needs
/// more of that patience, so the scale buys it in one place rather than at every call site.
pub fn budget(seconds: u64) -> Duration {
    Duration::from_secs(seconds * scale())
}

/// [`budget`] at millisecond granularity, for windows shorter than a second.
pub fn budget_millis(milliseconds: u64) -> Duration {
    Duration::from_millis(milliseconds * scale())
}

/// [`budget`] as an absolute instant, for `while Instant::now() < deadline` polling loops.
pub fn deadline(seconds: u64) -> Instant {
    Instant::now() + budget(seconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budgets_stay_proportional_whatever_the_ambient_scale() {
        // The scale is process-wide and comes from the environment, so a test may not assume a
        // particular value — only that it applies uniformly and never shortens a budget.
        assert_eq!(budget(6), budget(3) * 2);
        assert!(budget(1) >= Duration::from_secs(1));
    }
}
