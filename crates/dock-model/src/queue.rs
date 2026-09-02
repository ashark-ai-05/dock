//! The per-pane prompt queue, and every rule about when Dock is allowed to type into an agent
//! without a human present.
//!
//! This module owns the queue's state machine and nothing else: no runtime handle, no socket, no
//! PTY, and — deliberately — no clock. `poll` decides everything from its arguments, including
//! what time it is, so the entire safety surface can be tested without a process, a terminal, or a
//! daemon. That property is the point of the module rather than a convenience: this is the one
//! component that acts while nobody is watching, so the code that decides whether to act must be
//! reachable by a unit test with no moving parts in it.
//!
//! `dispatch.rs` owns the wiring — mapping a run to its pane and calling `pane_input` — and
//! `storage.rs` owns the file. What is here is the decision.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use dock_detect::{AgentKind, AgentState};

/// How long a pane must sit continuously non-`Working` before a queued prompt is fed.
///
/// This stacks on top of the screen classifier's own hysteresis rather than duplicating it, and
/// the two must be free to move apart. The classifier's dwell asks "has this answer held long
/// enough to be worth showing a person"; this asks the stricter question "has it held long enough
/// to be worth *acting on* while nobody is watching". A state good enough to paint in the roster
/// is not automatically good enough to send words to an agent.
pub const QUEUE_SETTLE: Duration = Duration::from_secs(3);

/// The floor between two feeds into the same pane.
///
/// Without it a detector flapping between `Working` and `Done` at tick rate could drain a whole
/// queue into an agent in a couple of seconds. With it, even a completely broken detector cannot
/// spend more than one entry per ten seconds — which is slow enough for a person to notice and
/// pause the daemon.
pub const QUEUE_MIN_INTERVAL: Duration = Duration::from_secs(10);

/// The most entries one pane may hold. Exceeding it is an error to the caller, never a silent
/// drop of the oldest: a queue that discards work is worse than one that says no.
pub const MAX_QUEUE_DEPTH: usize = 16;

/// The most entries the daemon may hold across every pane, mirroring `MAX_PANES_PER_WORKSPACE`.
///
/// A single `PaneQueue` cannot see the total, so this is enforced by whoever owns the map of
/// queues. It lives here so the number sits beside the per-pane cap it is proportioned against.
pub const MAX_QUEUED_TOTAL: usize = 64;

/// The largest prompt that may be queued. Over-long prompts are refused rather than truncated,
/// because half a prompt fed to an agent is worse than no prompt at all.
pub const MAX_PROMPT_BYTES: usize = 8192;

/// How much of a prompt a listing carries. A full listing of sixteen 8 KiB prompts across several
/// panes would exceed the protocol's message limit, so listings show a preview and the byte count.
pub const QUEUE_PREVIEW_BYTES: usize = 120;

/// Why arming is refused for an agent that has never said anything about itself.
///
/// Exported because the CLI, the daemon and the runs lane must all refuse in the same words: a
/// queue that is silently never going to fire is worse than one that refuses to be armed.
pub const ARM_WITHOUT_REPORTED_STATE: &str = "this agent has not reported its state; run `dock hooks --install` in its worktree, or start dockd with --auto-feed-trust=screen";

/// Why a restored queue comes back disarmed. See `PaneQueue::restored`.
pub const DISARMED_BY_RESTART: &str =
    "auto-feed was disarmed by a restart; arm it again when you are watching";

const HOLD_PAUSED: &str =
    "auto-feed is paused for the whole daemon; `dock queue resume` starts it again";
const HOLD_NOT_ARMED: &str = "auto-feed is not armed for this pane; `dock queue arm` turns it on";
const HOLD_NO_AGENT: &str = "no agent was detected in this pane, so auto-feed will not type a sentence at what may be a shell prompt";
const HOLD_AWAITING_ACK: &str = "fed a prompt that the agent has not started working on";
const HOLD_NEVER_WORKED: &str =
    "this pane has not been seen working, so there is no finished turn to feed after";
const HOLD_WORKING: &str = "the agent is still working";
const HOLD_BLOCKED: &str = "the agent is waiting on you, which is not the end of a turn";
const HOLD_NOT_AN_EDGE: &str =
    "this pane was already quiet before auto-feed saw it work, so no turn has ended here yet";
const HOLD_INFERRED: &str = "this done was inferred from the screen rather than reported by the agent; start dockd with --auto-feed-trust=screen to act on it";
const HOLD_SETTLING: &str = "waiting for the agent to stay finished for three seconds";
const HOLD_TOO_SOON: &str = "less than ten seconds since the last prompt was fed into this pane";

/// Which "the agent finished" signal auto-feed is willing to act on.
///
/// The screen classifier's only positive evidence that a turn ended is *silence*, so it calls a
/// pause of a second or two "finished". That is fine for painting a roster and wrong for typing
/// into an agent: nothing in the byte stream distinguishes an agent that is thinking from one that
/// is done, so a longer threshold trades false feeds for slow ones without ever reaching correct.
/// A state the agent reported about itself, through a hook, comes from the agent's own turn
/// boundaries and is the only signal trusted by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AutoFeedTrust {
    /// Only a state the agent reported through `dock agent-state`. The default.
    #[default]
    Reported,
    /// The screen classifier as well. Opt-in, for agents with no hooks.
    Screen,
}

/// One queued prompt.
///
/// `label` is what a listing calls the entry — a card title, usually. `prompt` is the literal text
/// fed to the agent; the daemon stores and replays text and does not know a task from a shopping
/// list, which is why resolving a card into words happens in the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueEntry {
    pub entry_id: u64,
    pub label: String,
    pub prompt: String,
}

impl QueueEntry {
    /// The first `QUEUE_PREVIEW_BYTES` of the prompt, cut at a character boundary.
    ///
    /// Cutting by bytes alone would panic on a prompt whose 120th byte lands inside a multi-byte
    /// character, which is an easy thing to hit with an em dash and a very silly way to lose a
    /// daemon.
    pub fn preview(&self) -> String {
        if self.prompt.len() <= QUEUE_PREVIEW_BYTES {
            return self.prompt.clone();
        }
        let mut end = QUEUE_PREVIEW_BYTES;
        while end > 0 && !self.prompt.is_char_boundary(end) {
            end -= 1;
        }
        self.prompt[..end].to_string()
    }

    /// The whole prompt's size, so a listing can say what its preview left out.
    pub fn bytes(&self) -> usize {
        self.prompt.len()
    }
}

/// One pane's queue of prompts, and the state machine that decides when one may be fed.
///
/// Deliberately holds no handle to a runtime, a PTY, or a clock — every rule below is decided by
/// `poll` from its arguments, so the whole safety surface is unit-testable without a process.
#[derive(Debug, Clone)]
pub struct PaneQueue {
    entries: VecDeque<QueueEntry>,
    next_entry_id: u64,
    /// Off unless a human armed it, and off again after any restart.
    ///
    /// There is no configuration key, environment variable, or flag that makes this true on a new
    /// queue. Queueing is harmless; auto-feeding is the one act that lets Dock work without a
    /// person present, so it is a deliberate one. Stated here so it is not optimised away later.
    auto_feed: bool,
    /// True from the moment a prompt is fed until the agent is next seen `Working`. While it is
    /// true nothing else is fed, so a misfire costs exactly one prompt rather than the whole
    /// queue — under a broken detector the worst case is one wrong prompt, ever, until a human
    /// looks.
    awaiting_ack: bool,
    /// Whether this pane has been `Working` at all since the last feed. Without it, a pane created
    /// beside a queue that already has entries would drain the whole thing before anyone had typed
    /// anything into it.
    seen_working_since_feed: bool,
    /// The last state observed, so a feed keys off a transition rather than a level.
    last_state: Option<AgentState>,
    /// When the current continuously-non-`Working` spell began, for the settle delay.
    settled_since: Option<Instant>,
    /// Whether that spell was entered directly from `Working` — the latched form of "a transition
    /// into done", which a per-tick comparison of the last two states cannot express once the
    /// settle delay makes the feed happen some ticks after the transition itself.
    settled_after_working: bool,
    last_fed_at: Option<Instant>,
    /// The entry handed out by the last `poll`, kept so a feed the daemon could not deliver can be
    /// put back rather than lost. See `feed_failed`.
    in_flight: Option<QueueEntry>,
    holding_because: Option<String>,
}

impl Default for PaneQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl PaneQueue {
    /// A new, empty, **disarmed** queue. There is no argument, constructor or default that makes
    /// `auto_feed` true here; arming is a separate deliberate act.
    pub fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            next_entry_id: 1,
            auto_feed: false,
            awaiting_ack: false,
            seen_working_since_feed: false,
            last_state: None,
            settled_since: None,
            settled_after_working: false,
            last_fed_at: None,
            in_flight: None,
            holding_because: None,
        }
    }

    /// A queue read back from disk.
    ///
    /// Entries survive a restart; **nothing else does**. `auto_feed` is forced off, and every
    /// observation — the last state, the settle clock, the acknowledgement flag — is discarded,
    /// because all of them describe the last few seconds of a process that no longer exists.
    /// Restoring them would let a pre-restart observation authorise a post-restart feed, and a
    /// daemon that comes back from a crash and immediately starts typing at agents is exactly the
    /// unattended behaviour the whole design guards against.
    pub fn restored(entries: Vec<QueueEntry>, next_entry_id: u64) -> Self {
        let highest = entries
            .iter()
            .map(|entry| entry.entry_id)
            .max()
            .unwrap_or(0);
        Self {
            entries: entries.into(),
            // A file that disagrees with itself must not hand out an id twice: two entries with
            // the same id makes `dock queue remove` ambiguous.
            next_entry_id: next_entry_id.max(highest.saturating_add(1)),
            holding_because: Some(DISARMED_BY_RESTART.to_string()),
            ..Self::new()
        }
    }

    /// Adds a prompt to the back of the queue, returning its entry id.
    ///
    /// Refuses rather than dropping or truncating: an over-full queue and an over-long prompt are
    /// both errors the caller can show, and neither silently loses work someone asked for.
    pub fn add(&mut self, label: String, prompt: String) -> Result<u64, String> {
        if prompt.trim().is_empty() {
            return Err("a queued prompt cannot be empty".to_string());
        }
        if prompt.len() > MAX_PROMPT_BYTES {
            return Err(format!(
                "a queued prompt may be at most {MAX_PROMPT_BYTES} bytes; this one is {}",
                prompt.len()
            ));
        }
        if self.entries.len() >= MAX_QUEUE_DEPTH {
            return Err(format!(
                "this pane already holds {MAX_QUEUE_DEPTH} queued prompts; remove one before adding another"
            ));
        }
        let entry_id = self.next_entry_id;
        self.next_entry_id = self.next_entry_id.saturating_add(1);
        self.entries.push_back(QueueEntry {
            entry_id,
            label,
            prompt,
        });
        Ok(entry_id)
    }

    /// Removes one entry by id, returning it. Naming the id rather than the position means a
    /// listing a person read a moment ago still refers to the same entry after a feed drained one.
    pub fn remove(&mut self, entry_id: u64) -> Result<QueueEntry, String> {
        let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.entry_id == entry_id)
        else {
            return Err(format!("this pane has no queued entry {entry_id}"));
        };
        Ok(self
            .entries
            .remove(index)
            .expect("index came from this queue"))
    }

    /// Empties the queue, returning how many entries went. Ids are not reused afterwards.
    pub fn clear(&mut self) -> usize {
        let removed = self.entries.len();
        self.entries.clear();
        self.holding_because = None;
        removed
    }

    pub fn entries(&self) -> &VecDeque<QueueEntry> {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn next_entry_id(&self) -> u64 {
        self.next_entry_id
    }

    pub fn auto_feed(&self) -> bool {
        self.auto_feed
    }

    pub fn awaiting_ack(&self) -> bool {
        self.awaiting_ack
    }

    /// Why auto-feed last declined to fire, as a sentence a listing can show verbatim. A stalled
    /// queue that explains itself is the difference between a safety guard and a bug report.
    pub fn holding_because(&self) -> Option<&str> {
        self.holding_because.as_deref()
    }

    /// Turns auto-feed on for this pane, or refuses.
    ///
    /// The precondition is guard (4) applied at arm time: under the default trust setting, a pane
    /// whose agent has never reported a state can never satisfy the feed rule, so arming it would
    /// produce a queue that sits there forever looking broken. Failing loudly here, naming the
    /// command that fixes it, is the whole reason `arm` is its own verb and not a flag on `add`.
    ///
    /// Whether the agent has ever reported is a fact the daemon holds, so it is passed in — this
    /// module still decides nothing from anything but its arguments.
    pub fn arm(
        &mut self,
        agent_has_reported_state: bool,
        trust: AutoFeedTrust,
    ) -> Result<(), String> {
        if trust == AutoFeedTrust::Reported && !agent_has_reported_state {
            return Err(ARM_WITHOUT_REPORTED_STATE.to_string());
        }
        self.auto_feed = true;
        self.holding_because = None;
        Ok(())
    }

    /// Turns auto-feed off again. Always allowed, and never refuses: the way out is never gated.
    pub fn disarm(&mut self) {
        self.auto_feed = false;
        self.holding_because = None;
    }

    /// The daemon could not deliver the prompt `poll` just handed out.
    ///
    /// Two things follow. The entry goes back to the front of the queue, because the work was
    /// never delivered and a queue that discards work is worse than one that says no. And the pane
    /// is **disarmed**: retrying into a pane whose binding just changed is how one wrong feed
    /// becomes many, so a failure asks for a human rather than another attempt.
    pub fn feed_failed(&mut self, message: &str) {
        if let Some(entry) = self.in_flight.take() {
            self.entries.push_front(entry);
        }
        self.auto_feed = false;
        self.awaiting_ack = false;
        self.holding_because = Some(message.to_string());
    }

    /// Everything auto-feed decides, decided here, from arguments. Returns the bytes to feed, or
    /// `None` with `holding_because` set to a sentence the runs lane can show verbatim.
    ///
    /// The trigger is `AgentState::Done`, and six conditions must all hold. Each of them exists
    /// because of a specific way that signal is wrong:
    ///
    /// 1. **Edge, not level.** The current quiet spell must have been entered from `Working`. A
    ///    level trigger would refeed on every tick while an agent sat waiting.
    /// 2. **The pane must have been `Working` since the last feed**, or a pane created next to a
    ///    queue with entries in it drains the whole thing before anyone has typed anything.
    /// 3. **There must be an agent.** A resolved `Idle`, or no detected agent, means a plain
    ///    shell. Feeding one would type a sentence at a `$` prompt and press return; it is the
    ///    sharpest hazard in the design and gets its own refusal rather than a silent skip.
    /// 4. **The `Done` must be hook-reported**, unless the user opted into trusting the screen.
    /// 5. **`QUEUE_SETTLE` of continuous quiet**, so a momentary misclassification is a non-event.
    /// 6. **`QUEUE_MIN_INTERVAL` since the last feed**, so a flapping detector cannot drain a
    ///    queue.
    ///
    /// Plus `awaiting_ack`, which is what makes the whole thing self-limiting: nothing more is fed
    /// until the agent is seen `Working`, so a queue whose prompt went somewhere unexpected stalls
    /// visibly instead of piling on.
    ///
    /// `paused` is the daemon-wide kill switch and overrides arming entirely.
    pub fn poll(
        &mut self,
        agent: Option<AgentKind>,
        state: AgentState,
        reported: bool,
        trust: AutoFeedTrust,
        paused: bool,
        now: Instant,
    ) -> Option<String> {
        let previous = self.last_state.replace(state);

        // Observation is unconditional: a queue that is paused, disarmed or empty still watches,
        // so that arming one does not need a history it was not keeping.
        if state == AgentState::Working {
            self.seen_working_since_feed = true;
            self.awaiting_ack = false;
            self.settled_since = None;
            self.settled_after_working = false;
        } else if self.settled_since.is_none() {
            self.settled_since = Some(now);
            self.settled_after_working = previous == Some(AgentState::Working);
        }

        if self.entries.is_empty() {
            // Nothing was declined, so nothing is being held. Clearing here stops a drained queue
            // from displaying the last complaint it had a reason to make.
            self.holding_because = None;
            return None;
        }
        if paused {
            return self.hold(HOLD_PAUSED);
        }
        if !self.auto_feed {
            return self.hold(HOLD_NOT_ARMED);
        }
        // Guard (3), before anything about turns: whether this is even an agent does not depend on
        // what state it is supposedly in, and the answer wants saying out loud.
        if agent.is_none() || state == AgentState::Idle {
            return self.hold(HOLD_NO_AGENT);
        }
        if self.awaiting_ack {
            return self.hold(HOLD_AWAITING_ACK);
        }
        // Guard (2).
        if !self.seen_working_since_feed {
            return self.hold(HOLD_NEVER_WORKED);
        }
        if state != AgentState::Done {
            return self.hold(match state {
                AgentState::Blocked => HOLD_BLOCKED,
                _ => HOLD_WORKING,
            });
        }
        // Guard (1).
        if !self.settled_after_working {
            return self.hold(HOLD_NOT_AN_EDGE);
        }
        // Guard (4).
        if !reported && trust == AutoFeedTrust::Reported {
            return self.hold(HOLD_INFERRED);
        }
        // Guard (5).
        let settled_for = self
            .settled_since
            .map(|since| now.saturating_duration_since(since))
            .unwrap_or_default();
        if settled_for < QUEUE_SETTLE {
            return self.hold(HOLD_SETTLING);
        }
        // Guard (6).
        if let Some(last_fed_at) = self.last_fed_at
            && now.saturating_duration_since(last_fed_at) < QUEUE_MIN_INTERVAL
        {
            return self.hold(HOLD_TOO_SOON);
        }

        let entry = self.entries.pop_front()?;
        self.awaiting_ack = true;
        self.seen_working_since_feed = false;
        // The spell that authorised this feed is spent. Belt and braces beside `awaiting_ack`:
        // even if the agent were somehow seen `Working` for one tick and quiet again, the next
        // feed needs a fresh transition of its own.
        self.settled_after_working = false;
        self.last_fed_at = Some(now);
        self.holding_because = None;
        let prompt = format!("{}\n", entry.prompt);
        self.in_flight = Some(entry);
        Some(prompt)
    }

    fn hold(&mut self, reason: &str) -> Option<String> {
        self.holding_because = Some(reason.to_string());
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TICK: Duration = Duration::from_millis(250);

    fn seconds(base: Instant, seconds: u64) -> Instant {
        base + Duration::from_secs(seconds)
    }

    /// An armed queue holding one prompt, which is the starting point of nearly every guard test.
    fn armed_with_one_prompt() -> PaneQueue {
        let mut queue = PaneQueue::new();
        queue
            .add("card 7".to_string(), "keep going".to_string())
            .expect("a first entry fits");
        queue
            .arm(true, AutoFeedTrust::Reported)
            .expect("a hooked agent can be armed");
        queue
    }

    /// A reported `Done`, armed, unpaused — the shape of a poll that is expected to feed.
    fn poll_done(queue: &mut PaneQueue, now: Instant) -> Option<String> {
        queue.poll(
            Some(AgentKind::Claude),
            AgentState::Done,
            true,
            AutoFeedTrust::Reported,
            false,
            now,
        )
    }

    fn poll_working(queue: &mut PaneQueue, now: Instant) -> Option<String> {
        queue.poll(
            Some(AgentKind::Claude),
            AgentState::Working,
            true,
            AutoFeedTrust::Reported,
            false,
            now,
        )
    }

    #[test]
    fn a_new_queue_is_disarmed_and_empty() {
        let queue = PaneQueue::new();
        assert!(
            !queue.auto_feed(),
            "nothing but a human may arm a queue, so a fresh one is never armed"
        );
        assert!(queue.is_empty());
        assert_eq!(queue.holding_because(), None);
    }

    #[test]
    fn a_queue_feeds_the_first_entry_when_a_working_agent_reports_it_is_done() {
        let base = Instant::now();
        let mut queue = armed_with_one_prompt();
        assert_eq!(poll_working(&mut queue, base), None);
        assert_eq!(
            poll_done(&mut queue, seconds(base, 1)),
            None,
            "still settling"
        );
        assert_eq!(
            poll_done(&mut queue, seconds(base, 5)),
            Some("keep going\n".to_string()),
            "the queue supplies the trailing newline itself"
        );
        assert!(queue.is_empty());
        assert_eq!(queue.holding_because(), None);
    }

    // ---- Guard (1): edge, not level ------------------------------------------------------

    #[test]
    fn a_pane_that_was_already_done_before_anyone_watched_is_never_fed_however_long_it_waits() {
        let base = Instant::now();
        let mut queue = armed_with_one_prompt();
        // Guard (2) is satisfied outright, so that this test turns on guard (1) alone: the pane
        // has worked at some point, but the quiet spell now on screen was not entered from
        // Working while this queue was watching. That is the difference between an edge and a
        // level, and it is set directly because every route through `poll` that reaches this
        // state also trips guard (2) — which is the point of having both.
        queue.seen_working_since_feed = true;
        for tick in 0..400 {
            assert_eq!(
                poll_done(&mut queue, base + TICK * tick),
                None,
                "a level trigger would have refed on every one of these ticks"
            );
        }
        assert_eq!(queue.holding_because(), Some(HOLD_NOT_AN_EDGE));
        assert_eq!(queue.len(), 1);

        // And the same queue, once a real transition into quiet happens, does feed.
        assert_eq!(poll_working(&mut queue, seconds(base, 200)), None);
        assert_eq!(poll_done(&mut queue, seconds(base, 201)), None);
        assert!(
            poll_done(&mut queue, seconds(base, 210)).is_some(),
            "the test is not vacuous: an edge feeds"
        );
    }

    #[test]
    fn a_one_frame_flicker_to_done_does_not_feed_the_queue() {
        let base = Instant::now();
        let mut queue = armed_with_one_prompt();
        for tick in 0..200 {
            let now = base + TICK * tick;
            let fed = if tick % 2 == 0 {
                poll_working(&mut queue, now)
            } else {
                poll_done(&mut queue, now)
            };
            assert_eq!(fed, None, "a single tick of Done never survives the settle");
        }
        assert_eq!(queue.len(), 1);
    }

    // ---- Guard (2): the pane must have been Working ---------------------------------------

    #[test]
    fn a_queue_does_not_feed_a_pane_that_has_never_been_working() {
        let base = Instant::now();
        let mut queue = armed_with_one_prompt();
        // A pane that came up beside a queue that already had entries in it. It is reporting Done
        // because its agent's TUI is drawn and waiting, and nobody has typed anything yet.
        for tick in 0..400 {
            assert_eq!(poll_done(&mut queue, base + TICK * tick), None);
        }
        assert_eq!(queue.holding_because(), Some(HOLD_NEVER_WORKED));
        assert_eq!(queue.len(), 1, "the whole queue is still there");

        // Not vacuous: one turn of real work is all that was missing.
        assert_eq!(poll_working(&mut queue, seconds(base, 200)), None);
        assert_eq!(poll_done(&mut queue, seconds(base, 201)), None);
        assert_eq!(
            poll_done(&mut queue, seconds(base, 210)),
            Some("keep going\n".to_string())
        );
    }

    // ---- Guard (3): a shell is not an agent -----------------------------------------------

    #[test]
    fn a_shell_pane_with_no_agent_is_never_auto_fed() {
        let base = Instant::now();
        let mut queue = armed_with_one_prompt();
        // A shell can look busy while a command runs and quiet afterwards, which is a Working to
        // Done transition by every other rule in the machine.
        assert_eq!(
            queue.poll(
                None,
                AgentState::Working,
                true,
                AutoFeedTrust::Reported,
                false,
                base
            ),
            None
        );
        for tick in 0..200 {
            assert_eq!(
                queue.poll(
                    None,
                    AgentState::Idle,
                    true,
                    AutoFeedTrust::Reported,
                    false,
                    seconds(base, 1) + TICK * tick,
                ),
                None,
                "typing a sentence at a shell prompt and pressing return is the sharpest hazard here"
            );
        }
        assert_eq!(queue.holding_because(), Some(HOLD_NO_AGENT));
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn a_resolved_idle_refuses_by_name_even_when_an_agent_was_detected() {
        let base = Instant::now();
        let mut queue = armed_with_one_prompt();
        assert_eq!(poll_working(&mut queue, base), None);
        assert_eq!(
            queue.poll(
                Some(AgentKind::Claude),
                AgentState::Idle,
                true,
                AutoFeedTrust::Reported,
                false,
                seconds(base, 10),
            ),
            None
        );
        assert_eq!(
            queue.holding_because(),
            Some(HOLD_NO_AGENT),
            "the refusal is explicit and its own sentence, not a silent skip"
        );

        // Not vacuous: the identical sequence with Done instead of Idle feeds.
        let mut queue = armed_with_one_prompt();
        assert_eq!(poll_working(&mut queue, base), None);
        assert_eq!(poll_done(&mut queue, seconds(base, 1)), None);
        assert_eq!(
            poll_done(&mut queue, seconds(base, 10)),
            Some("keep going\n".to_string())
        );
    }

    // ---- Guard (4): hook-reported, unless the screen is trusted ---------------------------

    #[test]
    fn a_screen_inferred_done_does_not_feed_the_queue_under_the_default_trust_setting() {
        let base = Instant::now();
        let mut queue = armed_with_one_prompt();
        assert_eq!(poll_working(&mut queue, base), None);
        for tick in 0..200 {
            assert_eq!(
                queue.poll(
                    Some(AgentKind::Claude),
                    AgentState::Done,
                    false,
                    AutoFeedTrust::Reported,
                    false,
                    seconds(base, 1) + TICK * tick,
                ),
                None,
                "silence is the classifier's only evidence a turn ended, which is not evidence"
            );
        }
        assert_eq!(queue.holding_because(), Some(HOLD_INFERRED));
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn a_screen_inferred_done_feeds_the_queue_when_the_user_opted_into_trusting_the_screen() {
        let base = Instant::now();
        let mut queue = PaneQueue::new();
        queue
            .add("card 7".to_string(), "keep going".to_string())
            .expect("a first entry fits");
        queue
            .arm(false, AutoFeedTrust::Screen)
            .expect("trusting the screen is what makes an unhooked agent armable");
        assert_eq!(
            queue.poll(
                Some(AgentKind::Claude),
                AgentState::Working,
                false,
                AutoFeedTrust::Screen,
                false,
                base
            ),
            None
        );
        assert_eq!(
            queue.poll(
                Some(AgentKind::Claude),
                AgentState::Done,
                false,
                AutoFeedTrust::Screen,
                false,
                seconds(base, 1),
            ),
            None
        );
        assert_eq!(
            queue.poll(
                Some(AgentKind::Claude),
                AgentState::Done,
                false,
                AutoFeedTrust::Screen,
                false,
                seconds(base, 10),
            ),
            Some("keep going\n".to_string()),
            "the same inputs that hold under Reported feed under Screen"
        );
    }

    #[test]
    fn arming_a_pane_whose_agent_has_never_reported_a_state_names_the_hooks_command() {
        let mut queue = PaneQueue::new();
        let refusal = queue
            .arm(false, AutoFeedTrust::Reported)
            .expect_err("a queue that can never fire refuses to be armed");
        assert!(refusal.contains("dock hooks --install"), "{refusal}");
        assert!(!queue.auto_feed());
        queue
            .arm(true, AutoFeedTrust::Reported)
            .expect("a hooked agent arms");
        assert!(queue.auto_feed());
    }

    // ---- Guard (5): the settle delay -------------------------------------------------------

    #[test]
    fn a_done_that_has_not_held_for_three_seconds_is_not_yet_worth_acting_on() {
        let base = Instant::now();
        let mut queue = armed_with_one_prompt();
        assert_eq!(poll_working(&mut queue, base), None);
        // The settle clock runs from the first tick on which the quiet was actually observed,
        // because that is the only moment the machine can honestly claim to know about.
        assert_eq!(poll_done(&mut queue, base), None);
        assert_eq!(
            poll_done(&mut queue, base + Duration::from_millis(2_999)),
            None,
            "one millisecond short of the settle is short of the settle"
        );
        assert_eq!(queue.holding_because(), Some(HOLD_SETTLING));
        assert_eq!(
            poll_done(&mut queue, base + QUEUE_SETTLE),
            Some("keep going\n".to_string()),
            "and the same state one millisecond later feeds"
        );
    }

    #[test]
    fn the_settle_clock_runs_from_the_start_of_the_quiet_spell_not_from_the_last_poll() {
        let base = Instant::now();
        let mut queue = armed_with_one_prompt();
        assert_eq!(poll_working(&mut queue, base), None);
        // Blocked is non-Working, so the spell starts here and Done inherits its clock.
        assert_eq!(
            queue.poll(
                Some(AgentKind::Claude),
                AgentState::Blocked,
                true,
                AutoFeedTrust::Reported,
                false,
                base,
            ),
            None
        );
        assert!(
            poll_done(&mut queue, seconds(base, 4)).is_some(),
            "four seconds of continuous quiet is four seconds however the states within it read"
        );
    }

    // ---- Guard (6): the minimum interval ---------------------------------------------------

    #[test]
    fn two_feeds_into_the_same_pane_are_at_least_ten_seconds_apart() {
        let base = Instant::now();
        let mut queue = PaneQueue::new();
        queue.add("one".to_string(), "first".to_string()).unwrap();
        queue.add("two".to_string(), "second".to_string()).unwrap();
        queue.arm(true, AutoFeedTrust::Reported).unwrap();

        assert_eq!(poll_working(&mut queue, base), None);
        assert_eq!(poll_done(&mut queue, base), None);
        assert_eq!(
            poll_done(&mut queue, seconds(base, 4)),
            Some("first\n".to_string())
        );
        // A perfectly well-behaved second turn, but too soon.
        assert_eq!(poll_working(&mut queue, seconds(base, 5)), None);
        assert_eq!(poll_done(&mut queue, seconds(base, 5)), None);
        assert_eq!(poll_done(&mut queue, seconds(base, 9)), None);
        assert_eq!(queue.holding_because(), Some(HOLD_TOO_SOON));
        assert_eq!(
            poll_done(&mut queue, seconds(base, 14)),
            Some("second\n".to_string()),
            "and ten seconds after the first feed, the second goes"
        );
    }

    // ---- awaiting_ack: the promise that a misfire costs one prompt -------------------------

    #[test]
    fn a_queue_feeds_nothing_more_until_the_agent_is_seen_working_again() {
        let base = Instant::now();
        let mut queue = PaneQueue::new();
        for n in 0..8 {
            queue
                .add(format!("card {n}"), format!("prompt {n}"))
                .unwrap();
        }
        queue.arm(true, AutoFeedTrust::Reported).unwrap();
        assert_eq!(poll_working(&mut queue, base), None);
        assert_eq!(poll_done(&mut queue, base), None);
        assert_eq!(
            poll_done(&mut queue, seconds(base, 4)),
            Some("prompt 0\n".to_string())
        );
        // The agent never picks it up: the feed went somewhere unexpected, or it was never really
        // finished. Hours of Done follow.
        for tick in 0..2_000 {
            assert_eq!(
                poll_done(&mut queue, seconds(base, 5) + TICK * tick),
                None,
                "one wrong prompt is the worst case, ever, until a human looks"
            );
        }
        assert_eq!(queue.holding_because(), Some(HOLD_AWAITING_ACK));
        assert_eq!(queue.len(), 7);

        // Not vacuous: the acknowledgement is a single observation of Working away.
        assert_eq!(poll_working(&mut queue, seconds(base, 600)), None);
        assert_eq!(poll_done(&mut queue, seconds(base, 601)), None);
        assert_eq!(
            poll_done(&mut queue, seconds(base, 610)),
            Some("prompt 1\n".to_string())
        );
    }

    #[test]
    fn a_queue_that_is_holding_explains_itself_in_one_sentence() {
        let base = Instant::now();
        let mut queue = PaneQueue::new();
        queue.add("card".to_string(), "prompt".to_string()).unwrap();
        assert_eq!(poll_done(&mut queue, base), None);
        let reason = queue.holding_because().expect("a held queue says why");
        assert!(reason.contains("dock queue arm"), "{reason}");
        assert!(!reason.contains('\n'), "one sentence, not a paragraph");
    }

    // ---- Arming, pausing, and the restart rule ---------------------------------------------

    #[test]
    fn a_queue_with_entries_feeds_nothing_until_its_pane_is_armed() {
        let base = Instant::now();
        let mut queue = PaneQueue::new();
        queue
            .add("card".to_string(), "keep going".to_string())
            .unwrap();
        assert_eq!(poll_working(&mut queue, base), None);
        assert_eq!(poll_done(&mut queue, seconds(base, 10)), None);
        assert_eq!(queue.holding_because(), Some(HOLD_NOT_ARMED));

        queue.arm(true, AutoFeedTrust::Reported).unwrap();
        assert_eq!(poll_working(&mut queue, seconds(base, 20)), None);
        assert_eq!(poll_done(&mut queue, seconds(base, 21)), None);
        assert_eq!(
            poll_done(&mut queue, seconds(base, 25)),
            Some("keep going\n".to_string()),
            "arming is the only thing that was missing"
        );
    }

    #[test]
    fn a_paused_daemon_feeds_nothing_however_armed_a_pane_is() {
        let base = Instant::now();
        let mut queue = armed_with_one_prompt();
        assert_eq!(poll_working(&mut queue, base), None);
        for tick in 0..200 {
            assert_eq!(
                queue.poll(
                    Some(AgentKind::Claude),
                    AgentState::Done,
                    true,
                    AutoFeedTrust::Reported,
                    true,
                    seconds(base, 1) + TICK * tick,
                ),
                None
            );
        }
        assert_eq!(queue.holding_because(), Some(HOLD_PAUSED));
        assert!(queue.auto_feed(), "pausing does not disarm anything");
        assert_eq!(
            poll_done(&mut queue, seconds(base, 200)),
            Some("keep going\n".to_string()),
            "resuming feeds the pane that was armed all along"
        );
    }

    #[test]
    fn disarming_stops_a_queue_that_was_about_to_feed() {
        let base = Instant::now();
        let mut queue = armed_with_one_prompt();
        assert_eq!(poll_working(&mut queue, base), None);
        queue.disarm();
        assert_eq!(poll_done(&mut queue, seconds(base, 10)), None);
        assert_eq!(queue.holding_because(), Some(HOLD_NOT_ARMED));
    }

    #[test]
    fn auto_feed_is_off_after_a_restart_even_if_it_was_armed_before() {
        let base = Instant::now();
        let mut before = armed_with_one_prompt();
        assert_eq!(poll_working(&mut before, base), None);
        assert!(before.auto_feed());

        // Only the entries cross the restart, which is all `DurablePaneQueue` carries.
        let entries: Vec<QueueEntry> = before.entries().iter().cloned().collect();
        let mut after = PaneQueue::restored(entries, before.next_entry_id());
        assert!(!after.auto_feed(), "a restart is not a human");
        assert_eq!(after.holding_because(), Some(DISARMED_BY_RESTART));
        assert_eq!(
            poll_done(&mut after, seconds(base, 10)),
            None,
            "a pre-restart observation must not authorise a post-restart feed"
        );
        assert_eq!(after.len(), 1);
    }

    #[test]
    fn a_restored_queue_never_hands_out_an_id_it_already_holds() {
        let restored = PaneQueue::restored(
            vec![QueueEntry {
                entry_id: 9,
                label: "card".to_string(),
                prompt: "prompt".to_string(),
            }],
            // A file that disagrees with itself, however it got that way.
            1,
        );
        assert_eq!(restored.next_entry_id(), 10);
    }

    // ---- Feed failure ----------------------------------------------------------------------

    #[test]
    fn a_failed_feed_disarms_the_pane_and_says_why() {
        let base = Instant::now();
        let mut queue = armed_with_one_prompt();
        assert_eq!(poll_working(&mut queue, base), None);
        assert_eq!(poll_done(&mut queue, seconds(base, 1)), None);
        assert_eq!(
            poll_done(&mut queue, seconds(base, 10)),
            Some("keep going\n".to_string())
        );
        queue.feed_failed("pane p2 is not bound to a live run");

        assert!(
            !queue.auto_feed(),
            "retrying is how one wrong feed becomes many"
        );
        assert_eq!(
            queue.holding_because(),
            Some("pane p2 is not bound to a live run")
        );
        assert_eq!(queue.len(), 1, "the undelivered prompt is not lost");
        assert_eq!(queue.entries()[0].prompt, "keep going");
    }

    // ---- The ordinary operations -----------------------------------------------------------

    #[test]
    fn a_queue_refuses_a_seventeenth_entry_rather_than_dropping_the_first() {
        let mut queue = PaneQueue::new();
        for n in 0..MAX_QUEUE_DEPTH {
            queue
                .add(format!("card {n}"), format!("prompt {n}"))
                .expect("the first sixteen fit");
        }
        let refusal = queue
            .add("card 16".to_string(), "prompt 16".to_string())
            .expect_err("the seventeenth is refused");
        assert!(refusal.contains("remove one"), "{refusal}");
        assert_eq!(queue.len(), MAX_QUEUE_DEPTH);
        assert_eq!(
            queue.entries()[0].prompt,
            "prompt 0",
            "the oldest entry is exactly what a dropping queue would have lost"
        );
    }

    #[test]
    fn a_prompt_over_the_byte_limit_is_refused_rather_than_truncated() {
        let mut queue = PaneQueue::new();
        let refusal = queue
            .add("card".to_string(), "x".repeat(MAX_PROMPT_BYTES + 1))
            .expect_err("an over-long prompt is refused");
        assert!(refusal.contains("8192"), "{refusal}");
        assert!(queue.is_empty());
        queue
            .add("card".to_string(), "x".repeat(MAX_PROMPT_BYTES))
            .expect("exactly the limit fits");
    }

    #[test]
    fn an_empty_prompt_is_refused_because_feeding_one_submits_a_blank_turn() {
        let mut queue = PaneQueue::new();
        queue
            .add("card".to_string(), "   \n".to_string())
            .expect_err("whitespace is not a prompt");
        assert!(queue.is_empty());
    }

    #[test]
    fn entries_are_removed_by_id_so_a_listing_read_a_moment_ago_still_means_something() {
        let mut queue = PaneQueue::new();
        let first = queue.add("a".to_string(), "one".to_string()).unwrap();
        let second = queue.add("b".to_string(), "two".to_string()).unwrap();
        assert_eq!(queue.remove(first).unwrap().prompt, "one");
        assert!(
            queue.remove(first).is_err(),
            "removing it twice is an error"
        );
        assert_eq!(queue.entries()[0].entry_id, second);
        assert_eq!(queue.clear(), 1);
        assert!(queue.is_empty());
        assert_eq!(
            queue.add("c".to_string(), "three".to_string()).unwrap(),
            3,
            "ids are not reused after a clear"
        );
    }

    #[test]
    fn a_preview_is_cut_at_a_character_boundary_rather_than_at_a_byte() {
        let entry = QueueEntry {
            entry_id: 1,
            // The multi-byte character straddles the 120-byte mark.
            label: "card".to_string(),
            prompt: format!("{}—{}", "x".repeat(119), "y".repeat(50)),
        };
        let preview = entry.preview();
        assert_eq!(preview, "x".repeat(119));
        assert_eq!(entry.bytes(), 119 + 3 + 50);

        let short = QueueEntry {
            entry_id: 2,
            label: "card".to_string(),
            prompt: "short".to_string(),
        };
        assert_eq!(short.preview(), "short");
    }

    // ---- The headline promise, as a property ------------------------------------------------

    /// The central claim of the whole design, driven rather than asserted case by case: whatever
    /// the detector does, a prompt fed is followed by no other prompt until the agent has actually
    /// been seen working. A broken detector costs one prompt, not a queue.
    #[test]
    fn however_the_detector_flaps_no_second_prompt_is_fed_before_the_agent_is_seen_working() {
        let base = Instant::now();
        let mut queue = PaneQueue::new();
        for n in 0..MAX_QUEUE_DEPTH {
            queue
                .add(format!("card {n}"), format!("prompt {n}"))
                .unwrap();
        }
        queue.arm(true, AutoFeedTrust::Reported).unwrap();

        // An adversarial detector: single-tick flickers, long runs of quiet that ought to feed,
        // Idle stretches where the pane looks like a shell, and a third of every state arriving
        // with no hook behind it. Written as (state, ticks it is held for) so the runs are long
        // enough that the sequence really can feed — a script that could never feed would prove
        // nothing at all.
        let script: Vec<AgentState> = [
            (AgentState::Working, 3),
            (AgentState::Done, 1),
            (AgentState::Working, 2),
            (AgentState::Done, 20),
            (AgentState::Blocked, 4),
            (AgentState::Idle, 2),
            (AgentState::Done, 8),
            (AgentState::Working, 1),
            (AgentState::Done, 40),
            (AgentState::Idle, 6),
            (AgentState::Working, 1),
            (AgentState::Done, 2),
        ]
        .into_iter()
        .flat_map(|(state, held)| std::iter::repeat_n(state, held))
        .collect();

        let mut fed = 0usize;
        let mut working_since_feed = true;
        let mut last_feed_at: Option<Instant> = None;

        for tick in 0..20_000u32 {
            let now = base + TICK * tick;
            let state = script[(tick as usize) % script.len()];
            let agent = if state == AgentState::Idle {
                None
            } else {
                Some(AgentKind::Claude)
            };
            let reported = tick % 3 != 0;
            if state == AgentState::Working {
                working_since_feed = true;
            }
            if queue
                .poll(agent, state, reported, AutoFeedTrust::Reported, false, now)
                .is_some()
            {
                assert!(
                    working_since_feed,
                    "a prompt was fed at tick {tick} without the agent having worked since the last one"
                );
                if let Some(previous) = last_feed_at {
                    assert!(
                        now.saturating_duration_since(previous) >= QUEUE_MIN_INTERVAL,
                        "two feeds at tick {tick} closer together than the minimum interval"
                    );
                }
                working_since_feed = false;
                last_feed_at = Some(now);
                fed += 1;
            }
        }
        assert!(fed > 0, "the sequence must be capable of feeding at all");
        assert!(
            queue.is_empty(),
            "and of draining, given a cooperative agent"
        );
        assert_eq!(fed, MAX_QUEUE_DEPTH);
    }

    /// The same drive with the two failures that matter most: a detector that never reports, and an
    /// agent that never picks anything up. Neither may spend more than one prompt.
    #[test]
    fn an_agent_that_never_starts_working_costs_exactly_one_prompt_and_a_never_reported_done_costs_none()
     {
        let base = Instant::now();

        let mut never_acknowledges = PaneQueue::new();
        for n in 0..MAX_QUEUE_DEPTH {
            never_acknowledges
                .add(format!("card {n}"), format!("prompt {n}"))
                .unwrap();
        }
        never_acknowledges
            .arm(true, AutoFeedTrust::Reported)
            .unwrap();
        // One turn of real work, then the agent goes quiet forever: it never picks the fed prompt
        // up, so the detector's Done is a lie from here on.
        assert_eq!(poll_working(&mut never_acknowledges, base), None);
        let mut fed = 0usize;
        for tick in 0..20_000u32 {
            if poll_done(&mut never_acknowledges, seconds(base, 1) + TICK * tick).is_some() {
                fed += 1;
            }
        }
        assert_eq!(fed, 1, "the worst case is one wrong prompt, ever");
        assert_eq!(never_acknowledges.len(), MAX_QUEUE_DEPTH - 1);
        assert_eq!(
            never_acknowledges.holding_because(),
            Some(HOLD_AWAITING_ACK)
        );

        let mut never_reports = PaneQueue::new();
        for n in 0..MAX_QUEUE_DEPTH {
            never_reports
                .add(format!("card {n}"), format!("prompt {n}"))
                .unwrap();
        }
        never_reports.arm(true, AutoFeedTrust::Reported).unwrap();
        for tick in 0..20_000u32 {
            let now = base + TICK * tick;
            let state = if tick % 7 == 0 {
                AgentState::Working
            } else {
                AgentState::Done
            };
            assert_eq!(
                never_reports.poll(
                    Some(AgentKind::Claude),
                    state,
                    false,
                    AutoFeedTrust::Reported,
                    false,
                    now,
                ),
                None,
                "an inferred Done is never enough on its own"
            );
        }
        assert_eq!(never_reports.len(), MAX_QUEUE_DEPTH);
    }
}
