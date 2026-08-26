use std::{borrow::Cow, collections::HashMap, path::Path, time::Instant};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    buffer::{Buffer, Cell},
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use tui_term::widget::{Cursor, PseudoTerminal};

use crate::{
    adapter::{AdapterId, AdapterSelection},
    board::{BoardTask, BoardView},
    clipboard::{self, ClipboardRoute},
    copy::{CopySession, find_matches},
    detect::{AgentKind, AgentState},
    files,
    git::GitFacts,
    keymap::{FocusDirection, KeyOutcome, Keymap, PaneCommand},
    layout::{
        LayoutNode, LayoutSnapshot, PaneKind, PaneLayout, PaneRuntime, SplitAxis, WorkspaceLayout,
    },
    model::{HandoffRecord, ReviewDecision, ReviewRoute},
    picker::{Picker, PickerItem},
    protocol::{
        BindingKind, DashboardProfile, DecideRequest, DispatchRequest, Event,
        LaunchIntoPaneRequest, PROTOCOL_VERSION, PaneHistoryRequest, PaneQueueSnapshot,
        QueueRequest, Request, Response, RuntimeSnapshot, TerminalLaunchRequest, WorkspaceRequest,
    },
    terminal::{KeyEncoding, PANE_HISTORY_BYTES, PaneScreen, PaneSnapshot, encode_paste},
    theme::Theme,
};

/// How much older output one page-back asks for.
///
/// Deliberately far below what a pane retains. Extending a replica's history means replaying
/// every byte it holds through a fresh parser, because a parser cannot be prepended to, and
/// that cost is paid on the wheel notch that asks for it. A chunk this size is thousands of
/// rows — several screens of scrolling — for a rebuild that stays inside a frame.
const PANE_PAGE_BACK_BYTES: u32 = 2 << 20;

/// How far a pane's byte log is allowed past its budget before it is trimmed back to it.
///
/// Trimming is a memmove of the whole log, and at the 16 MiB budget that is a third of a
/// millisecond — paid on the event-drain path, ahead of render, for a daemon that pushes every
/// 16 ms. Trimming on every delta would spend it on every delta. Letting the log run a
/// megabyte past its budget and then cutting all the way back spends it once per megabyte of
/// output instead, which is hundreds of deltas, in exchange for a megabyte of headroom per
/// pane. The daemon's own `OutputLog` sidesteps the copy entirely with a `VecDeque` of whole
/// writes; a client's log is replayed as one contiguous slice, so it buys the same amortisation
/// with slack rather than with a second data structure.
const PANE_HISTORY_TRIM_SLACK: usize = 1 << 20;

/// What a pane's replica holds, and where it would ask for more.
///
/// The bytes live here rather than in a map of their own because every part of this is written
/// together and read together: a cursor that named a position in a log it was not attached to
/// would be the one bug this whole mechanism can produce.
///
/// `from` is the sequence `log` begins at, which is exactly the cursor a `PaneHistory` request
/// names. `epoch` is the byte stream those sequences belong to: a run that restarted has a new
/// one, and an answer carrying the old one names positions in a stream this replica is no
/// longer showing. `complete` is the daemon saying there is nothing older still retained.
struct PaneHistoryCursor {
    epoch: u64,
    from: u64,
    complete: bool,
    /// Set once this log has dropped bytes off its front, after which `from` no longer names
    /// where it starts and no request can be built from it. See `retain_history_bytes`.
    wrapped: bool,
    /// Set once a page-back answer arrived and left the replica holding no more rows than it
    /// held before, which means asking again cannot help.
    ///
    /// The row-capacity and headroom stops in `history_request_for` both read
    /// `history_rows()`, and `history_rows()` reads the *active* grid. A pane in the alternate
    /// screen has no scrollback there at all — `vt100` builds the alternate grid with
    /// `scrollback_len = 0` — so it answers 0 whatever the pane has printed, and both stops
    /// silently invert: nothing is ever "enough rows", and there are never rows "above the
    /// viewport" to be far from. That is vim, less, htop, and the agent TUIs that are most of
    /// what Dock runs, and it is not only them: a progress bar or a spinner repainting in
    /// place produces bytes without producing rows. Left alone, every wheel notch pays a
    /// two-megabyte round trip and a rebuild of a log that has just grown by two megabytes,
    /// for output the pane can never display.
    ///
    /// So the answer itself is the evidence: if a page-back did not raise the row count, this
    /// pane stops asking. A fresh `PaneAttached` builds a new cursor and therefore clears it,
    /// which is the right reset — a re-attach is the one event that can change the answer.
    fruitless: bool,
    /// Every byte this replica has been fed, seed and deltas alike.
    ///
    /// Kept because a `vt100` parser cannot have rows prepended to it: the only way older
    /// output enters a replica is to replay it, followed by everything the replica already
    /// saw, through a brand new parser.
    log: Vec<u8>,
}

/// Copy mode's bindings, published in the footer for as long as the mode is active. It is
/// the only way in without reading the help, and the only reminder of the way out.
const COPY_HINTS: &str =
    "hjkl move \u{b7} v select \u{b7} y yank \u{b7} / search \u{b7} n/N next/prev \u{b7} Esc exit";

const MIN_PANE_WIDTH: u16 = 8;
const MIN_PANE_HEIGHT: u16 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiCommand {
    Request(Box<Request>),
    /// A request whose answer nobody needs: painted, posted, and not waited on.
    ///
    /// `Request` blocks on the daemon and then refreshes, which is four round trips and
    /// possibly a `ps`. That is right when the answer is the product — a queue listing, a
    /// page of history. It is wrong for a change the dashboard has already made locally and
    /// is only telling the daemon about, because there the waiting is pure latency in front
    /// of whatever gesture the user is mid-way through. `PaneResize` already goes this way
    /// for the same reason; a refused request is not lost, because the client counts unread
    /// replies and `take_deferred_error` surfaces them on the next drain.
    Send(Box<Request>),
    /// Several requests that only mean anything together, sent in order. Closing a workspace is
    /// the one command Dock has that cannot be expressed as a single `WorkspaceRequest`: the
    /// daemon drops a workspace when its last pane goes, so the workspace is closed by closing
    /// every pane in it, and a half-sent batch would leave a workspace nobody asked to keep.
    Requests(Vec<Request>),
    /// Raw bytes bound for the focused pane's PTY. Kept apart from `Request` because the render
    /// loop must send it without waiting for a reply: the echo comes back on the event stream,
    /// so blocking here would put a daemon round trip in front of every keystroke's paint.
    PaneInput(Vec<u8>),
    LoadCatalog,
    /// Asks the daemon for the pending handoffs. Distinct from `Request` because its response is
    /// the point: everything else only needs to know whether the daemon objected.
    LoadReviewInbox,
    /// Asks for the task board, which lives in the repository rather than the daemon.
    LoadBoard,
    /// Asks what Git says about the focused pane's worktree.
    LoadGit,
    /// Gives a task somewhere isolated to be worked on and launches an agent there. Carries the
    /// task rather than a worktree: the worktree may not exist yet, and making it is the point.
    DispatchTask(TaskDispatch),
    Refresh,
    Quit,
    None,
}

#[derive(Default)]
pub struct Dashboard {
    pub layout: LayoutSnapshot,
    pub runs: Vec<RuntimeSnapshot>,
    pub repository_root: String,
    pub runtime_directory: String,
    pub repository_launches: Vec<RepositoryLaunchOption>,
    pub workspace_index: usize,
    pub error: Option<String>,
    /// This client's own emulator for each run, advanced by pushed deltas. The daemon holds the
    /// authoritative screen; this is the local replica the dashboard actually paints from.
    pub screens: HashMap<String, PaneScreen>,
    /// Each replica's own byte log, where it starts, and whether anything older is still to
    /// be had. Bounded by `PANE_HISTORY_BYTES + PANE_HISTORY_TRIM_SLACK` (the trim only fires
    /// once the log has passed the slack, see `retain_history_bytes`), so a pane that runs for
    /// a week costs the same as one that just started.
    history: HashMap<String, PaneHistoryCursor>,
    /// Latest agent identity and state per run, as pushed by the daemon.
    pub agents: HashMap<String, (Option<AgentKind>, AgentState)>,
    revisions: HashMap<String, u64>,
    /// Tasks dispatched to an agent whose command line had nowhere to carry them, waiting for
    /// that agent to be up enough to be typed into. Keyed by run, emptied on first delivery.
    opening_prompts: HashMap<String, OpeningPrompt>,
    /// Opening prompts whose moment arrived, waiting for the render loop to send them.
    pending_opening_prompts: Vec<(String, String, String)>,
    needs_refresh: bool,
    pending_resizes: Vec<(String, String, u16, u16)>,
    /// The run and inner geometry last announced for each pane, so an unchanged frame
    /// announces nothing.
    pane_geometry: HashMap<String, (String, u16, u16)>,
    keymap: Keymap,
    theme: Theme,
    /// Pane rendered alone at full size, if any. Purely local: the daemon's layout tree is
    /// untouched, so zooming costs no request.
    zoomed: Option<String>,
    pane_areas: HashMap<String, Rect>,
    /// The body of each pane, inside its border. Kept alongside `pane_areas` because the
    /// pointer-to-grid conversion for drag selection has to skip the border cells, and only
    /// the render pass knows where the border ended up.
    pane_inner_areas: HashMap<String, Rect>,
    dividers: Vec<Divider>,
    dragging: Option<DragTarget>,
    /// A left button held down inside a pane body, and the cell it went down on. Distinct
    /// from `dragging`, which is the divider gesture; a press lands on one or the other.
    pane_drag: Option<PaneDrag>,
    sequence: u64,
    launch_area: Option<Rect>,
    launch_form: Option<LaunchForm>,
    launch_profile_areas: Vec<Rect>,
    launch_confirm_area: Option<Rect>,
    launch_mode_area: Option<Rect>,
    help_open: bool,
    /// Copy mode, if active. Client-local: reading history costs the daemon nothing.
    copy: Option<CopyMode>,
    /// True only while copy mode's `/` prompt is taking characters. Kept beside `copy` rather
    /// than inside `CopySession` because the query outlives the prompt: `n`/`N` reuse it once
    /// Enter has closed the editor.
    copy_searching: bool,
    rename_form: Option<(RenameTarget, String)>,
    /// The open chooser, if any, and what taking a row from it will do. Client-local: filtering a
    /// list the daemon already sent costs the daemon nothing.
    picker: Option<(PickerPurpose, Picker)>,
    /// The review overlay, if open. Holds the handoffs an agent submitted and is waiting on a
    /// human for, which is the one queue in Dock that a person rather than a process drains.
    review: Option<ReviewOverlay>,
    /// The board as last read, kept so a taken row can be resolved back to the task it named.
    board_tasks: Vec<BoardTask>,
    /// Where that board was read from, and whether it is Dock's own rather than a repository's.
    /// Dock only ever writes tasks to its own.
    board_dir: Option<std::path::PathBuf>,
    board_is_personal: bool,
    /// The columns a Board *pane* draws, kept apart from `board` because a pane is not an
    /// overlay: nothing opens or closes it, Esc means nothing to it, and it must survive the
    /// overlay being opened and closed over the top of it. It renders the same `BoardView` the
    /// overlay does, which is the whole reason keeping the overlay costs nothing.
    board_pane_view: Option<BoardView>,
    /// Every pane queue the daemon holds, as of the last refresh.
    ///
    /// Queue depth lives only in the daemon — nothing the client can see implies it — so this is
    /// replicated state like `agents` is, refilled by `refresh` and invalidated by
    /// `Event::QueueChanged`. Held as the wire listing rather than indexed by pane: it is capped
    /// at `MAX_QUEUED_TOTAL` entries across a handful of panes, and a map rebuilt per frame would
    /// cost more than the scan it saves.
    queues: Vec<PaneQueueSnapshot>,
    /// The daemon-wide kill switch, as the daemon last reported it. Independent of every pane's
    /// own arming, so the lane must be able to say "armed, and paused anyway".
    queues_paused: bool,
    /// The board's one cursor: which column, and what in it.
    ///
    /// One grid means one cursor. There used to be two — a lane cursor keyed by pane and the
    /// view's own column/row — and deleting the lane without merging them would have left `a`
    /// and `>` pointing at different things on the same screen.
    ///
    /// The target is named rather than numbered for the reason the lane's cursor was: `ACTIVE`
    /// sorts blocked agents to the top, so an agent going `Blocked` two rows down would slide a
    /// different pane under an index — and this is the cursor that arms an agent. `None` means
    /// the cursor has not been moved yet and the view's own opening position stands.
    ///
    /// The inner `Option` is a column with nothing in it, which is a place the cursor is allowed
    /// to be: a board whose todo column is empty must still be walkable across, and refusing to
    /// enter one would make `l` skip a column silently.
    board_cursor: Option<(usize, Option<BoardTarget>)>,
    #[cfg(test)]
    /// Stands in for what is installed on this machine. Tests must not ask the machine, or they
    /// assert something about the laptop they ran on rather than about Dock.
    pub(crate) installed_adapters: Option<Vec<AdapterId>>,
    /// Which task each run was dispatched onto, for runs this dashboard dispatched.
    ///
    /// A repository-bound run carries its task in the binding and the daemon reports it, so this
    /// is only needed for unbound ones: `TerminalLaunchRequest` has no task field, so nothing
    /// durable records the pairing. Client-local, and lost with the dashboard — which is honest,
    /// because it is a note about what this dashboard did rather than a fact about the run.
    /// The Git overlay, if open.
    git: Option<GitOverlay>,
    /// The board, if open.
    board: Option<BoardOverlay>,
    picker_row_areas: Vec<Rect>,
    /// Where each workspace's tab landed, so the strip is clickable like every other chrome.
    tab_areas: Vec<(String, Rect)>,
    /// The index of the first workspace the strip is currently showing.
    ///
    /// Bounds-clamped on every render so a resize or a closed workspace cannot strand it past
    /// the end, but *not* re-anchored to the active tab every render — see `tab_scroll_last_active`
    /// and `render_tabs`. A wheel scroll is the one thing allowed to leave the active tab
    /// scrolled out of view, and it does that by changing only this field.
    tab_scroll: usize,
    /// The workspace that was active the last time the strip was rendered.
    ///
    /// Exists to tell a jump from a plain re-render: `render_tabs` scrolls the active tab back
    /// into view only when `workspace_index` differs from this, then updates it to match. A
    /// wheel scroll never touches `workspace_index`, so it never triggers the correction — which
    /// is the point, and why this clamp does not simply run every frame like the bounds one
    /// above it. Do not "fix" it into an unconditional clamp; that regresses the wheel to a
    /// no-op, which is the exact bug this field exists to avoid.
    tab_scroll_last_active: usize,
    /// The whole tab-strip row, recorded so the wheel arm can tell a scroll over the strip from
    /// a scroll over a pane without duplicating the layout the strip's own render already did.
    tab_strip_area: Option<Rect>,
    /// `‹`/`›`, drawn in the strip's reserved edge columns only when tabs are hidden on that
    /// side, and clickable like every other tab-strip control.
    tab_scroll_left_area: Option<Rect>,
    tab_scroll_right_area: Option<Rect>,
    /// The `+` at the end of the tab strip, and the rename and close affordances on the
    /// active tab.
    new_workspace_area: Option<Rect>,
    rename_workspace_area: Option<Rect>,
    close_workspace_area: Option<Rect>,
    /// Where the armed close's confirm target is. Separate from the cancel target because a
    /// destructive answer must not share a cell with the click that asked the question.
    confirm_close_workspace_area: Option<Rect>,
    /// The workspace whose close control has been clicked once and is waiting to be clicked
    /// again. Held as an id rather than a flag so a refresh that reorders or drops workspaces
    /// cannot turn a primed confirmation into consent to destroy a different one.
    close_workspace_armed: Option<String>,
    /// Split and close controls on the focused pane's own border.
    pane_control_areas: Vec<(PaneControl, Rect)>,
    /// The sidebar's clickable menu of what this dashboard can do.
    quick_action_areas: Vec<(PaneCommand, Rect)>,
    last_launch_profile: usize,
    last_repository_mode: bool,
    /// The last text this dashboard put on the clipboard, and what a middle or right click
    /// pastes back.
    ///
    /// Held in process rather than read back out of the OS clipboard, because reading it means
    /// `pbpaste`, and spawning a subprocess on the render thread to answer a click is exactly
    /// the stall this pass exists to remove. It is also what a middle click means everywhere
    /// else: X11's PRIMARY selection is the last thing *selected*, not the last thing copied
    /// with a keystroke, and it is per-application rather than global.
    last_copied: Option<String>,
    /// When and where the last left press inside a pane landed, and how many presses that run
    /// has reached, so double and triple click can be derived.
    last_click: Option<Click>,
    /// A divider drag's latest ratio, held until the button comes up.
    ///
    /// Sending it per motion event made every divider drag a stream of blocking daemon round
    /// trips — the resize, then `refresh`'s two more — so the divider moved in visible steps
    /// behind the pointer. The local layout is still updated on every event, so the drag looks
    /// live; only the authority call waits for the release.
    pending_divider_resize: Option<(String, String, u16)>,
}

/// A control drawn on the focused pane's border. Each mirrors a published key, so the mouse
/// reaches what the keyboard reaches rather than growing its own vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneControl {
    SplitHorizontal,
    SplitVertical,
    Rename,
    Close,
}

/// What a rename form is editing. The protocol already distinguishes these — `Rename` takes an
/// optional `pane_id` — but the keyboard path only ever renamed panes, so nothing produced the
/// workspace form until tabs became clickable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameTarget {
    Pane,
    Workspace,
}

/// One live agent pane, as it is right now.
///
/// Assembled per frame from the run list and the agent roster, and stored nowhere — because a
/// run that ends must leave the board the moment its run does, and anything cached would keep a
/// finished agent on screen until something thought to evict it. Borrowed from the dashboard's
/// own state rather than owned, because this is built on a path that runs at 60fps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveRun<'a> {
    pub run_id: &'a str,
    pub workspace_id: &'a str,
    pub pane_id: &'a str,
    pub agent: AgentKind,
    pub state: AgentState,
    /// The board card this run is bound to, when the daemon's binding says so.
    pub task_id: Option<u64>,
    /// How many prompts are waiting for this pane.
    pub queued: usize,
    pub auto_feed: bool,
    /// A prompt has been fed that the agent has not been seen working on yet. The queue is armed
    /// and will still not fire, which is a different thing from being idle and worth saying.
    pub awaiting_ack: bool,
    /// Why auto-feed last declined to fire, in the daemon's own words. Borrowed, because it is
    /// the daemon's sentence and rewording it here would give a stalled queue two explanations.
    pub holding_because: Option<&'a str>,
}

/// The status whose column is drawn as `ACTIVE`.
///
/// The constant, the file on disk and every `status:` line stay `in-progress`; only the heading
/// changes, because the column now holds live agents that have no card as well as the cards that
/// are in progress, and "in progress" is not what a hand-launched agent is.
const ACTIVE_STATUS: &str = "in-progress";

/// What the board's cursor is on: named, never numbered.
///
/// `ACTIVE` re-sorts itself as its agents change state, and the key on this cursor is the one
/// that lets Dock type into an agent unattended — so an index would silently point at a different
/// pane the moment an agent went `Blocked`. This is the rule the old runs lane's cursor was built
/// for, applied to the whole grid rather than to one strip of it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BoardTarget {
    Card(u64),
    /// A live agent with no card in `ACTIVE`, named by the pane it is in — the pane is what
    /// arming names, and it outlives the run that is in it.
    Pane(String, String),
}

/// One entry in the `ACTIVE` column.
///
/// Two shapes rather than one, because the column genuinely holds two kinds of thing: work the
/// board knows about, and work only the daemon knows about. Neither is ever written back — see
/// [`active_entries`].
#[derive(Debug, Clone, Copy)]
enum ActiveEntry<'a> {
    /// A card whose status is `in-progress`, joined to the run it was dispatched to if that run
    /// is still there. `None` is the card whose agent has gone, which is the card most worth
    /// showing and the one a hiding rule would hide.
    Card(&'a BoardTask, Option<&'a LiveRun<'a>>),
    /// A live agent with no card in this column: launched by hand, or bound to a card that is
    /// somewhere else on the board.
    Loose(&'a LiveRun<'a>),
}

impl<'a> ActiveEntry<'a> {
    fn run(self) -> Option<&'a LiveRun<'a>> {
        match self {
            Self::Card(_, run) => run,
            Self::Loose(run) => Some(run),
        }
    }

    fn target(self) -> BoardTarget {
        match self {
            Self::Card(task, _) => BoardTarget::Card(task.id),
            Self::Loose(run) => {
                BoardTarget::Pane(run.workspace_id.to_owned(), run.pane_id.to_owned())
            }
        }
    }

    /// Attention first, matching `attention_rank` — then the agent's name and its id, so two
    /// equally urgent entries do not swap places between frames. A card with no live run sorts
    /// last: there is nothing happening on it to look at.
    fn rank(self) -> (u8, &'a str, u64, &'a str) {
        match self.run() {
            Some(run) => (
                run.state.attention_rank(),
                run.agent.label(),
                run.task_id.unwrap_or(u64::MAX),
                run.pane_id,
            ),
            None => (u8::MAX, "", self.target_id(), ""),
        }
    }

    fn target_id(self) -> u64 {
        match self {
            Self::Card(task, _) => task.id,
            Self::Loose(_) => u64::MAX,
        }
    }
}

/// The live half of the board, joined to the cards once per frame.
///
/// The join is by `external_task_ref`, which is the daemon's own record of which card a run was
/// dispatched onto — the same join the runs lane did, kept because it is the only thing that can
/// say a card and an agent are the same work.
struct BoardLive<'a> {
    runs: &'a [LiveRun<'a>],
    /// The run on each card, for the badge every column draws and the entry `ACTIVE` draws.
    /// A map rather than a scan per card: a busy canvas has dozens of agents and a board has
    /// hundreds of cards, and this is a path that repaints at 60fps.
    by_task: HashMap<u64, &'a LiveRun<'a>>,
}

impl<'a> BoardLive<'a> {
    fn new(runs: &'a [LiveRun<'a>]) -> Self {
        Self {
            runs,
            by_task: runs
                .iter()
                .filter_map(|run| Some((run.task_id?, run)))
                .collect(),
        }
    }
}

/// The `ACTIVE` column: the cards that are in progress, and every live agent that is not on one.
///
/// Derived on every frame and written nowhere. That is the rule this whole column rests on: the
/// status detector calls a 1.8-second pause "finished", and a derived column that gets an agent
/// wrong shows one wrong card for one frame and then corrects itself, where a column that wrote
/// what it derived would leave a wrong `status:` line on disk for somebody to find tomorrow.
///
/// Nothing appears twice: a card carries its own run, so the run is not also listed as an agent
/// with no card. Nothing disappears either — an agent whose card is in another column is still
/// listed here, because that card's badge cannot be armed and an agent nobody can reach is an
/// agent nobody can disarm.
fn active_entries<'a>(view: &'a BoardView, live: &'a BoardLive<'a>) -> Vec<ActiveEntry<'a>> {
    let cards = view.cards(ACTIVE_STATUS);
    let mut entries: Vec<ActiveEntry<'a>> = cards
        .iter()
        .map(|task| ActiveEntry::Card(task, live.by_task.get(&task.id).copied()))
        .chain(
            live.runs
                .iter()
                .filter(|run| {
                    run.task_id
                        .is_none_or(|task| !cards.iter().any(|card| card.id == task))
                })
                .map(ActiveEntry::Loose),
        )
        .collect();
    entries.sort_by(|left, right| left.rank().cmp(&right.rank()));
    entries
}

/// The board, laid out as columns of cards.
#[derive(Debug, Clone)]
pub struct BoardOverlay {
    pub view: BoardView,
    pub directory: std::path::PathBuf,
    /// Whether Dock may write here. A repository's board is read on the same screen but never
    /// altered, so the controls that would change it are not offered.
    pub writable: bool,
    /// A title being typed. While this is `Some`, every printable key belongs to it.
    pub composing: Option<String>,
}

/// What Git says about a worktree, and how far into the diff the reader is.
#[derive(Debug, Clone)]
pub struct GitOverlay {
    pub facts: GitFacts,
    pub diff: Vec<String>,
    pub scroll: usize,
}

/// A task that has to be typed to the agent it was dispatched to, and where to type it.
///
/// Amp and Copilot have no prompt positional, so a dispatched card reached them as an empty pane
/// and the task existed only in the head of whoever pressed the key. The pane is remembered
/// alongside the run because the prompt has to reach *that* pane whether or not it still has
/// focus by the time the agent finishes starting.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OpeningPrompt {
    workspace_id: String,
    pane_id: String,
    prompt: String,
}

/// Everything needed to give a task a worktree and put an agent on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskDispatch {
    pub workspace_id: String,
    pub pane_id: String,
    pub run_id: String,
    pub task_id: u64,
    pub title: String,
    /// The agent to put on it, which is whichever one was launched last. Carried rather than
    /// re-derived so the dispatch cannot silently disagree with what the launch form shows.
    pub adapter: AdapterId,
}

/// The pending handoffs and where the reviewer is inside them.
#[derive(Debug, Clone, Default)]
pub struct ReviewOverlay {
    /// Every handoff and the decision it has already had, if any. Answered ones are kept rather
    /// than filtered away: "what did that agent actually produce" is asked long after the queue
    /// is drained, and a queue that forgets is no record at all.
    pub items: Vec<(HandoffRecord, Option<ReviewDecision>)>,
    pub selected: usize,
    /// The route chosen and the note being typed for it. A decision without a note is refused by
    /// `ReviewDecision::new`, so the note is collected before anything is sent rather than after
    /// the daemon rejects it.
    pub pending: Option<(ReviewRoute, String)>,
}

/// One of the surfaces Dock can put over the canvas.
///
/// Each still owns its own state on `Dashboard` and its own hand-written `render_*`/`key_*`
/// pair; this names them so the *order* they are drawn and routed in can be stated once,
/// instead of once per site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayKind {
    Help,
    Rename,
    LaunchForm,
    Picker,
    Review,
    Board,
    Git,
    Copy,
}

/// Every overlay, in the one order that governs both drawing and key routing.
///
/// Drawing and key routing were two hardcoded lists of the same eight surfaces, written in two
/// different orders. Drawing went `launch, help, rename, picker, review, git, board`; routing
/// went `help, rename, launch, picker, review, board, git, copy`. The launch form was drawn
/// before help and rename but routed after them, and the board was drawn after the Git overlay
/// but routed before it. Nothing was ever observable, because at most one overlay is open at a
/// time — which is precisely the hazard: the disagreement could not be caught by using Dock, and
/// the ninth surface would have inherited whichever list its author happened to read.
///
/// Both sites now derive from this array, so adding a surface is one entry in one place and
/// there is no second list to get half right.
const OVERLAY_ORDER: [OverlayKind; 8] = [
    OverlayKind::Help,
    OverlayKind::Rename,
    OverlayKind::LaunchForm,
    OverlayKind::Picker,
    OverlayKind::Review,
    OverlayKind::Board,
    OverlayKind::Git,
    OverlayKind::Copy,
];

/// What an open picker does with the row the user takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerPurpose {
    Workspace,
    File,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryLaunchOption {
    pub task_ref: String,
    pub worktree: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LaunchForm {
    index: usize,
    repository_mode: bool,
    confirming: bool,
    query: String,
}

/// The sidebar menu: the things worth doing from a dashboard with nothing running yet, each with
/// the key that also does it. Deliberately short — a menu of everything is a menu of nothing.
const QUICK_ACTIONS: [(&str, &str, PaneCommand); 4] = [
    ("Ctrl+B k", "task board", PaneCommand::Board),
    ("Ctrl+B f", "find a file", PaneCommand::FilePicker),
    ("Ctrl+B g", "what changed", PaneCommand::Git),
    ("Ctrl+B ?", "every key", PaneCommand::Help),
];

const PROFILES: &[(DashboardProfile, &str)] = &[
    (DashboardProfile::Fixture, "Fixture"),
    (DashboardProfile::Amp, "Amp"),
    (DashboardProfile::ClaudeCode, "Claude Code"),
    (DashboardProfile::CodexCli, "Codex CLI"),
    (DashboardProfile::GithubCopilotCli, "GitHub Copilot CLI"),
];

#[derive(Debug, Clone)]
struct Divider {
    area: Rect,
    pane_id: String,
    axis: SplitAxis,
    container: Rect,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DragTarget {
    pane_id: String,
    axis: SplitAxis,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PaneDrag {
    run_id: String,
    /// Grid cell the press landed on, which becomes the selection anchor.
    origin: (u16, u16),
    /// The pane body at press time, so the pointer can be mapped to cells for the rest of
    /// the gesture even if a later frame moves the pane.
    inner: Rect,
    /// Whether *this* gesture actually put text under a selection — by dragging, or by a
    /// double or triple click. Release copies only when it did, so a plain click can never
    /// re-copy a selection left standing from an earlier gesture and overwrite whatever the
    /// user has put on their clipboard since.
    selected: bool,
}

/// The previous left press, for deriving click counts.
///
/// crossterm reports presses, never how many of them arrived in a row, so double and triple
/// click have to be inferred from the time and place of the last one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Click {
    at: Instant,
    column: u16,
    row: u16,
    count: u8,
}

/// How long after a press a second one still counts as part of the same click.
///
/// 450ms sits inside the range every desktop uses (macOS defaults to 500ms, GTK to 400ms) and
/// is long enough that a deliberate double click is never missed, short enough that two
/// separate clicks on the same word are not fused into one.
const MULTI_CLICK_WINDOW: std::time::Duration = std::time::Duration::from_millis(450);

/// An open copy mode: the selection, and the screen it is a selection *of*.
///
/// The two are one object because a selection means nothing apart from the exact grid it was
/// made against. Copy mode used to hold only the session and read the pane's live screen, so
/// every pushed delta moved the text under coordinates that had already been chosen — the
/// mode called itself frozen and was not. Freezing is this clone and nothing else: the live
/// parser is never interrupted, so on exit there is no backlog to replay and the live screen
/// is already current.
///
/// Not `Clone`, because [`PaneSnapshot`] is not. That is deliberate rather than incidental:
/// the render path wants this object on every frame for every pane, and a session that could
/// be cloned there would copy the whole grid and scrollback sixty times a second.
struct CopyMode {
    session: CopySession,
    /// The pane's screen as it stood when the mode opened. Rendered from, selected from, and
    /// scrolled through for as long as the mode lasts; dropping it is the entire exit path.
    frozen: PaneSnapshot,
}

impl CopyMode {
    fn new(run_id: String, cursor: (u16, u16), frozen: PaneSnapshot) -> Self {
        Self {
            session: CopySession::new(run_id, cursor),
            frozen,
        }
    }

    fn is_for(&self, run_id: &str) -> bool {
        self.session.run_id == run_id
    }

    /// Moves the copy cursor, pulling the frozen viewport through scrollback when it walks off
    /// an edge so the cursor never leaves the rows on screen.
    ///
    /// The scrollback travels with the clone, so this walks back through history that new
    /// output can no longer move underneath it.
    fn step(&mut self, rows: i32, cols: i32, bounds: (u16, u16)) {
        let (row, _) = self.session.cursor();
        let edge = if rows < 0 && row == 0 {
            1
        } else if rows > 0 && row + 1 >= bounds.0 {
            -1
        } else {
            0
        };
        if edge != 0 {
            let before = self.frozen.scroll_offset();
            self.frozen.scroll_by(edge);
            // Only the anchor. The cursor is what the user is deliberately moving, and it is
            // pinned to the edge by the clamp in `move_cursor` below — which is exactly how
            // `k` past the top row walks into history a row at a time.
            self.session
                .shift_anchor(scrolled(before, self.frozen.scroll_offset()), bounds);
        }
        self.session.move_cursor(rows, cols, bounds);
    }
}

impl Dashboard {
    pub fn set_repository_catalog(
        &mut self,
        repository_root: String,
        repository_launches: Vec<RepositoryLaunchOption>,
    ) {
        self.repository_root = repository_root;
        self.repository_launches = repository_launches;
        if self.repository_launches.is_empty()
            && let Some(form) = self.launch_form.as_mut()
        {
            form.repository_mode = false;
        }
    }

    /// Feeds a pushed event into this client's own emulator.
    ///
    /// `PaneAttached` is always a (re-)seed: the daemon sends it when a run is first seen and
    /// again whenever the pane's geometry changes, so the parser is rebuilt at the announced
    /// `rows`/`cols` rather than reused. Keeping the old parser would silently render the
    /// snapshot at the wrong width.
    ///
    /// A non-contiguous revision means this client missed bytes, so the screen is dropped
    /// rather than advanced into a corrupted grid.
    pub fn apply_event(&mut self, event: Event) {
        match event {
            Event::PaneAttached {
                run_id,
                revision,
                rows,
                cols,
                scrollback_rows,
                history_from,
                epoch,
                screen,
            } => {
                // The daemon's own retention, so this replica holds exactly the history the
                // daemon holds. The capacity is fixed for a `vt100` terminal's lifetime, and a
                // replica built with none would leave the wheel (`Dashboard::mouse`'s
                // `ScrollUp`/`ScrollDown` arm) nothing to scroll into however much the pane
                // produced.
                let mut terminal = PaneScreen::new(rows, cols, scrollback_rows as usize);
                let seed = STANDARD.decode(&screen).unwrap_or_default();
                terminal.feed(&seed);
                self.screens.insert(run_id.clone(), terminal);
                // The seed is a replay of the daemon's log from `history_from`, so that is
                // where this replica's own history begins and what anything older is *before*.
                // A re-attach replaces both together: the frame is a fresh replay, so a log
                // kept from the previous one would be replayed twice.
                self.history.insert(
                    run_id.clone(),
                    PaneHistoryCursor {
                        epoch,
                        from: history_from,
                        complete: false,
                        wrapped: false,
                        fruitless: false,
                        log: seed,
                    },
                );
                self.revisions.insert(run_id.clone(), revision);
                self.end_copy_mode_for(&run_id, "the pane was re-attached");
            }
            Event::PaneDelta {
                run_id,
                revision,
                bytes,
            } => {
                let expected = self.revisions.get(&run_id).map(|value| value + 1);
                if expected != Some(revision) {
                    self.screens.remove(&run_id);
                    self.revisions.remove(&run_id);
                    self.history.remove(&run_id);
                    self.end_copy_mode_for(&run_id, "the pane lost sync and is re-seeding");
                    return;
                }
                if let (Some(terminal), Ok(decoded)) =
                    (self.screens.get_mut(&run_id), STANDARD.decode(&bytes))
                {
                    terminal.feed(&decoded);
                    self.revisions.insert(run_id.clone(), revision);
                    self.retain_history_bytes(&run_id, &decoded);
                }
            }
            Event::AgentStateChanged {
                run_id,
                agent,
                state,
            } => {
                // An agent reaching `Done` for the first time is saying its input box is up and
                // waiting, which is the earliest moment a task can be typed into it. Taken from
                // the map rather than read, so a later turn — every one of which ends in `Done` —
                // never sends the task a second time.
                if state == AgentState::Done
                    && let Some(opening) = self.opening_prompts.remove(&run_id)
                {
                    self.pending_opening_prompts.push((
                        opening.workspace_id,
                        opening.pane_id,
                        opening.prompt,
                    ));
                }
                self.agents.insert(run_id, (agent, state));
            }
            // Queue depth lives only in the daemon, so unlike agent state nothing else a
            // subscriber receives would tell the client a queue drained.
            Event::PaneState { .. } | Event::LayoutChanged | Event::QueueChanged { .. } => {
                self.needs_refresh = true
            }
        }
    }

    /// Drops every replicated screen, for use when the event stream is re-established. The
    /// fresh subscription re-attaches every live run with a full snapshot, so anything not
    /// re-attached belongs to a run that is gone and would otherwise be painted forever.
    pub fn detach_screens(&mut self) {
        self.screens.clear();
        self.revisions.clear();
        // The byte logs go with the parsers they were built to rebuild. A fresh subscription
        // re-attaches with a new seed and a new cursor, and a log kept across that would be
        // replayed in front of bytes it already contains.
        self.history.clear();
        // The frozen screen would happily outlive the replica it was cloned from — nothing in
        // it points back at one — but a selection over a pane that is about to be rebuilt from
        // a fresh snapshot is a selection of rows the user is no longer being shown.
        if let Some(run_id) = self.copy.as_ref().map(|mode| mode.session.run_id.clone()) {
            self.end_copy_mode_for(&run_id, "the connection was re-established");
        }
        // The agent roster is replicated state exactly like the screens are, and it is pushed
        // only when a run's identity or state *changes*. Left behind, every entry from before
        // the drop would keep painting a sidebar row for a run that may no longer exist.
        self.agents.clear();
    }

    /// Records bytes a replica has been fed, so its parser can be rebuilt from them later.
    ///
    /// Trimmed from the front, never the back: the newest bytes are the ones on screen, and a
    /// log missing its tail would rebuild a pane that has forgotten what it just printed. The
    /// trim runs only once the log has passed `PANE_HISTORY_TRIM_SLACK` beyond its budget and
    /// then cuts all the way back, so the copy is paid once per slack rather than once per
    /// delta.
    ///
    /// **A trim ends this pane's paging, and the arithmetic to avoid that does not exist.**
    /// The obvious repair — advance `from` by the bytes dropped — is wrong, and subtly:
    /// `from` counts *stream* sequence, while the log also holds the cursor-addressed
    /// corrections `SubscriberView::next_delta` appends (`src/server.rs`), which occupy log
    /// space and consume no sequence at all. Dropping `c` corrective bytes would leave `from`
    /// naming a sequence `c` past where the log truly starts, the daemon's next contiguous
    /// answer would overlap the log head by `c`, and the rebuild would replay those bytes
    /// twice into a screen the pane never showed. A client cannot tell the two kinds of byte
    /// apart, so the flag says so instead of guessing. Nothing is lost: reaching this point
    /// takes a whole budget of output, which is hundreds of thousands of rows, and the
    /// `PANE_HISTORY_MAX_ROWS` capacity stop has long since ended the paging anyway.
    fn retain_history_bytes(&mut self, run_id: &str, bytes: &[u8]) {
        let Some(cursor) = self.history.get_mut(run_id) else {
            return;
        };
        cursor.log.extend_from_slice(bytes);
        if cursor.log.len() <= PANE_HISTORY_BYTES + PANE_HISTORY_TRIM_SLACK {
            return;
        }
        cursor.log.drain(..cursor.log.len() - PANE_HISTORY_BYTES);
        cursor.wrapped = true;
    }

    /// A request for the next chunk of history, when this pane is scrolled near the top of
    /// what it holds and there is any point asking for more.
    ///
    /// Four separate things say there is no point. The daemon's `complete` means nothing
    /// older is retained. A replica already holding its full row budget means nothing older
    /// can be *shown*: `vt100` drops the oldest row for every row added, so those bytes would
    /// be replayed at the cost of a full rebuild and then immediately discarded. A log
    /// that has wrapped can no longer name the sequence it starts at, so there is no honest
    /// request to send — see `retain_history_bytes`. And a pane whose last answer raised no
    /// row has said, in the only way it can, that its rows are not going into scrollback at
    /// all — see `PaneHistoryCursor::fruitless`, which is what covers the alternate screen.
    ///
    /// Takes `&mut self` because `history_rows` does: reading the row count means moving the
    /// scroll offset to the clamp and putting it back.
    fn history_request_for(&mut self, run_id: &str) -> Option<Request> {
        if self.copy.as_ref().is_some_and(|mode| mode.is_for(run_id)) {
            // A frozen pane paints its snapshot, and rebuilding the live parser cannot add a
            // row to a snapshot that was cloned before it. Without this the wheel would fire a
            // request and a rebuild on every notch for as long as copy mode is open.
            return None;
        }
        let before = match self.history.get(run_id) {
            Some(cursor) if !cursor.complete && !cursor.wrapped && !cursor.fruitless => cursor.from,
            _ => return None,
        };
        let screen = self.screens.get_mut(run_id)?;
        let (rows, _) = screen.size();
        let held = screen.history_rows();
        if held >= screen.history_capacity() {
            return None;
        }
        // Rows still above the viewport. One screen height of headroom, so the request goes
        // out just before the user reaches the top rather than at the moment they hit it.
        let above = held.saturating_sub(screen.scroll_offset());
        if above > usize::from(rows) {
            return None;
        }
        Some(Request::PaneHistory(PaneHistoryRequest {
            run_id: run_id.to_owned(),
            before,
            max_bytes: PANE_PAGE_BACK_BYTES,
        }))
    }

    /// Splices older output in front of what a pane holds, by rebuilding its parser from the
    /// extended byte log.
    ///
    /// A parser cannot be prepended to, so this is the only way history enters a replica: a
    /// fresh parser, fed the older bytes and then every byte this replica had already seen.
    /// The viewport is preserved explicitly. `vt100` measures the scroll offset from the
    /// bottom, so rows added above happen to leave it pointing at the same content — but that
    /// is a property of the engine rather than of this code, and restoring the offset says so
    /// where a swap of the engine would notice.
    pub fn apply_pane_history_response(&mut self, response: Response) {
        let Response::PaneHistory {
            run_id,
            epoch,
            from,
            bytes,
            complete,
        } = response
        else {
            if let Response::Error { message, .. } = response {
                self.error = Some(message);
            }
            return;
        };
        // Every handle and every reason to give up comes first, so there is no ordering in
        // which the cursor advances over a log that was never extended.
        let Some(screen) = self.screens.get_mut(&run_id) else {
            return;
        };
        let Some(cursor) = self.history.get_mut(&run_id) else {
            return;
        };
        if cursor.epoch != epoch {
            // The pane restarted between the request and the answer. These bytes belong to a
            // stream this replica is not showing, and the sequences naming them mean nothing
            // in the one it is.
            return;
        }
        let Ok(older) = STANDARD.decode(&bytes) else {
            return;
        };
        debug_assert_eq!(
            from + older.len() as u64,
            cursor.from,
            "a page-back answer must abut the head of the log it is spliced in front of: \
             `OutputLog::before` returns a contiguous run ending exactly at the cursor it was \
             asked about, so a gap or an overlap means the pane was re-seeded between the \
             request and the answer while keeping its epoch. Nothing rejects that today; it is \
             prevented only by `main.rs`'s loop being synchronous, which this assert is here to \
             notice the loss of."
        );
        // Read before anything is spliced, so the verdict below compares the same pane's rows
        // before and after this answer rather than against a number carried from elsewhere.
        let held_before = screen.history_rows();
        cursor.from = from;
        cursor.complete = complete;
        // Not trimmed to `PANE_HISTORY_BYTES` here, unlike the live path: the front of this log
        // is exactly what was just asked for, and trimming it would discard the answer. The
        // total stays bounded anyway, because the daemon can only ever serve what it retains.
        cursor.log.splice(0..0, older);
        let (rows, cols) = screen.size();
        let offset = screen.scroll_offset();
        // Read off the screen being replaced rather than carried on the cursor, so the rebuild
        // cannot disagree with what it replaces about how much history a pane may keep.
        let mut rebuilt = PaneScreen::new(rows, cols, screen.history_capacity());
        rebuilt.feed(&cursor.log);
        // The answer is its own evidence. A page-back that bought no scrollback row bought
        // nothing at all, and the next one would buy nothing either: see
        // `PaneHistoryCursor::fruitless` for the alternate screen, which is where this happens
        // and where the two stops in `history_request_for` cannot see it.
        cursor.fruitless = rebuilt.history_rows() <= held_before;
        rebuilt.scroll_by(i32::try_from(offset).unwrap_or(i32::MAX));
        *screen = rebuilt;
    }

    /// Replaces the run list and drops any agent roster entry whose run is gone.
    ///
    /// `agents` is fed by pushed `AgentStateChanged` events, which are sent on change and never
    /// on disappearance: nothing else would ever remove an entry, so a session accumulated a
    /// dead row for every pane shell it ever retired. The authoritative run list is the only
    /// thing that knows a run has stopped existing, so pruning happens exactly where it lands.
    pub fn set_runs(&mut self, runs: Vec<RuntimeSnapshot>) {
        self.agents
            .retain(|run_id, _| runs.iter().any(|run| &run.run_id == run_id));
        // An agent that died before it ever finished starting is never going to be typed into,
        // and its task would otherwise sit here waiting for a `Done` that cannot arrive — and
        // then be delivered to whatever run next inherited the id.
        self.opening_prompts
            .retain(|run_id, _| runs.iter().any(|run| &run.run_id == run_id));
        self.runs = runs;
    }

    /// Remembers a task to type into an agent that had nowhere to carry it on its command line.
    ///
    /// Deliberately not queued as ordinary work: every guard on a queue is about an agent that is
    /// already running, and one still starting has satisfied none of them, so a queued opening
    /// prompt would wait for conditions that had already passed.
    pub fn expect_opening_prompt(
        &mut self,
        run_id: &str,
        workspace_id: &str,
        pane_id: &str,
        prompt: &str,
    ) {
        self.opening_prompts.insert(
            run_id.to_owned(),
            OpeningPrompt {
                workspace_id: workspace_id.to_owned(),
                pane_id: pane_id.to_owned(),
                prompt: prompt.to_owned(),
            },
        );
    }

    /// Opening prompts whose agent has come up, as `(workspace_id, pane_id, prompt)`.
    pub fn take_opening_prompts(&mut self) -> Vec<(String, String, String)> {
        std::mem::take(&mut self.pending_opening_prompts)
    }

    /// True once when a pushed event invalidated the run list or layout. The render loop uses
    /// this instead of an unconditional timer poll, so an idle dashboard issues no requests.
    pub fn take_refresh(&mut self) -> bool {
        std::mem::take(&mut self.needs_refresh)
    }

    /// Pane geometry changes the render pass discovered, as `(workspace_id, pane_id, rows, cols)`.
    /// Only genuinely changed geometry lands here: a resize per pane per frame would be a
    /// request storm on an otherwise idle socket.
    pub fn take_pending_resizes(&mut self) -> Vec<(String, String, u16, u16)> {
        std::mem::take(&mut self.pending_resizes)
    }

    /// True while `Ctrl+B` has been pressed and the dashboard is waiting for the command key.
    pub fn prefix_pending(&self) -> bool {
        self.keymap.is_pending()
    }

    /// The visible text of a run's replicated screen, sized from the parser's own geometry so a
    /// re-attach at a smaller pane does not read rows that no longer exist.
    pub fn screen_text(&self, run_id: &str) -> Option<String> {
        self.screens
            .get(run_id)
            .map(|screen| screen.text_tail(screen.size().0))
    }

    pub fn workspace(&self) -> Option<&WorkspaceLayout> {
        self.layout.workspaces.get(self.workspace_index)
    }

    /// Whether one named overlay is on screen right now.
    ///
    /// The single place that knows which field stands for which surface, so `OVERLAY_ORDER` can
    /// be a list of names rather than a list of conditions repeated at every site.
    fn overlay_is_open(&self, kind: OverlayKind) -> bool {
        match kind {
            OverlayKind::Help => self.help_open,
            OverlayKind::Rename => self.rename_form.is_some(),
            OverlayKind::LaunchForm => self.launch_form.is_some(),
            OverlayKind::Picker => self.picker.is_some(),
            OverlayKind::Review => self.review.is_some(),
            OverlayKind::Board => self.board.is_some(),
            OverlayKind::Git => self.git.is_some(),
            OverlayKind::Copy => self.copy.is_some(),
        }
    }

    /// The overlays that are open, in `OVERLAY_ORDER`. Drawing walks it; key routing takes its
    /// first entry.
    fn open_overlays(&self) -> impl Iterator<Item = OverlayKind> + '_ {
        OVERLAY_ORDER
            .into_iter()
            .filter(|kind| self.overlay_is_open(*kind))
    }

    /// Draws one overlay over the whole canvas.
    ///
    /// Copy mode draws nothing here, and that is deliberate rather than an omission: it is
    /// anchored to its pane's rectangle rather than to the screen's, so `render_node` paints it
    /// where the pane is. It keeps its place in `OVERLAY_ORDER` because the order is about
    /// precedence among open surfaces, and copy mode has one.
    fn render_overlay(&mut self, kind: OverlayKind, frame: &mut Frame, area: Rect) {
        match kind {
            OverlayKind::Help => self.render_help(frame, area),
            OverlayKind::Rename => self.render_rename(frame, area),
            OverlayKind::LaunchForm => self.render_launch_form(frame, area),
            OverlayKind::Picker => self.render_picker(frame, area),
            OverlayKind::Review => self.render_review(frame, area),
            OverlayKind::Board => self.render_board(frame, area),
            OverlayKind::Git => self.render_git(frame, area),
            OverlayKind::Copy => {}
        }
    }

    /// Hands one key to one overlay.
    ///
    /// Help is answered inline because it has no state to hold: it is a page, and the only keys
    /// it takes are the two that close it. Every other surface keeps the handler it already had.
    fn overlay_key(&mut self, kind: OverlayKind, key: KeyEvent) -> UiCommand {
        match kind {
            OverlayKind::Help => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Char('?')) {
                    self.help_open = false;
                }
                UiCommand::None
            }
            OverlayKind::Rename => self.rename_key(key),
            OverlayKind::LaunchForm => self.launch_key(key),
            OverlayKind::Picker => self.picker_key(key),
            OverlayKind::Review => self.review_key(key),
            OverlayKind::Board => self.board_key(key),
            OverlayKind::Git => self.git_key(key),
            OverlayKind::Copy => self.copy_key(key),
        }
    }

    pub fn render(&mut self, frame: &mut Frame) {
        self.pane_areas.clear();
        self.pane_inner_areas.clear();
        self.dividers.clear();
        self.launch_area = None;
        self.launch_profile_areas.clear();
        self.launch_confirm_area = None;
        self.launch_mode_area = None;
        self.picker_row_areas.clear();
        self.tab_areas.clear();
        self.tab_strip_area = None;
        self.tab_scroll_left_area = None;
        self.tab_scroll_right_area = None;
        self.new_workspace_area = None;
        self.rename_workspace_area = None;
        self.close_workspace_area = None;
        self.confirm_close_workspace_area = None;
        self.pane_control_areas.clear();
        let area = frame.area();
        // Painted first so every widget that leaves cells untouched still sits on the theme's
        // surface rather than whatever the host terminal happens to use.
        frame.render_widget(
            Block::default().style(Style::default().bg(self.theme.surface).fg(self.theme.text)),
            area,
        );
        if area.width < 52 || area.height < 14 {
            self.dragging = None;
            self.render_narrow(frame, area);
            return;
        }
        let header = Rect::new(area.x, area.y, area.width, 2);
        let footer = Rect::new(area.x, area.bottom().saturating_sub(2), area.width, 2);
        // The strip costs a row, so it is only taken when there is more than one workspace to
        // choose between. With a single workspace it would be a row spent saying "you are here".
        let tabs_height = u16::from(self.layout.workspaces.len() > 1);
        let tabs = Rect::new(area.x, area.y + 2, area.width, tabs_height);
        let body = Rect::new(
            area.x,
            area.y + 2 + tabs_height,
            area.width,
            area.height.saturating_sub(4 + tabs_height),
        );
        let sidebar_width = body.width.min(28);
        let sidebar = Rect::new(body.x, body.y, sidebar_width, body.height);
        let panes = Rect::new(
            body.x + sidebar_width,
            body.y,
            body.width - sidebar_width,
            body.height,
        );
        self.render_header(frame, header);
        if tabs_height > 0 {
            self.render_tabs(frame, tabs);
        }
        self.render_sidebar(frame, sidebar);
        if let Some(workspace) = self.workspace().cloned() {
            let zoomed = self
                .zoomed
                .clone()
                .filter(|pane_id| workspace.panes.contains_key(pane_id));
            match zoomed {
                Some(pane_id) => {
                    self.render_node(frame, panes, &workspace, &LayoutNode::Pane { pane_id })
                }
                None => self.render_node(frame, panes, &workspace, &workspace.root),
            }
        } else {
            frame.render_widget(
                Paragraph::new("No workspace yet. Press Ctrl+B n to create one.")
                    .style(Style::default().fg(self.theme.muted))
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_type(Theme::border_type())
                            .border_style(Style::default().fg(self.theme.border))
                            .title(" RUNTIME "),
                    ),
                panes,
            );
        }
        if self.dragging.as_ref().is_some_and(|target| {
            !self
                .dividers
                .iter()
                .any(|divider| divider.pane_id == target.pane_id && divider.axis == target.axis)
        }) {
            self.dragging = None;
        }
        // Drawn in `OVERLAY_ORDER`, later entries over earlier ones, so the sequence on screen is
        // the sequence the key router walks.
        //
        // Written as a loop over the constant rather than over `open_overlays` on purpose: this
        // runs on every frame, and the iterator borrows `self` immutably while each `render_*`
        // needs it back mutably, so the obvious spelling would have to collect into a `Vec` and
        // pay an allocation per frame for eight `Copy` discriminants. This compiles to the same
        // eight checks the hand-written chain was.
        for kind in OVERLAY_ORDER {
            if self.overlay_is_open(kind) {
                self.render_overlay(kind, frame, area);
            }
        }
        // The which-key bar publishes every binding, and there are now more bindings than two rows
        // can hold. Rather than shaving words off the table until the last entry fits — which
        // silently truncates and reads as a missing binding — it is drawn over the bottom of the
        // body while the prefix is held, and gives the rows back the moment it is released.
        let footer = if self.keymap.is_pending() {
            let wanted = footer.height.max(4).min(area.height);
            let taller = Rect::new(
                area.x,
                area.bottom().saturating_sub(wanted),
                area.width,
                wanted,
            );
            frame.render_widget(Clear, taller);
            frame.render_widget(
                Block::default().style(Style::default().bg(self.theme.surface)),
                taller,
            );
            taller
        } else {
            footer
        };
        frame.render_widget(
            Paragraph::new(self.footer_line()).wrap(Wrap { trim: true }),
            footer,
        );
    }

    /// The single footer line. While the prefix is pending it becomes a which-key bar, which
    /// is the only place the binding table is ever published without the user asking for help.
    fn footer_line(&self) -> Line<'static> {
        // Copy mode outranks the standing hints and carries any notice alongside itself
        // rather than being replaced by one: a modal mode that goes invisible the moment
        // something goes wrong is the failure P0 deleted the last input mode over.
        if let Some(status) = self.copy_status() {
            let mut spans = vec![
                Span::styled(
                    status,
                    Style::default()
                        .fg(self.theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" \u{b7} "),
            ];
            spans.push(match self.error.as_deref() {
                Some(error) => {
                    Span::styled(error.to_owned(), Style::default().fg(self.theme.blocked))
                }
                None => Span::styled(COPY_HINTS, Style::default().fg(self.theme.muted)),
            });
            return Line::from(spans);
        }
        if let Some(error) = self.error.as_deref() {
            return Line::styled(error.to_owned(), Style::default().fg(self.theme.blocked));
        }
        if self.keymap.is_pending() {
            return Line::from(
                Keymap::hints()
                    .iter()
                    .flat_map(|(key, action)| {
                        [
                            Span::styled(
                                format!(" {key} "),
                                Style::default()
                                    .fg(self.theme.accent)
                                    .add_modifier(Modifier::BOLD),
                            ),
                            // No trailing space: the key span already opens with one, and the
                            // doubled gap cost more columns than the two-row footer has.
                            Span::styled(action.to_owned(), Style::default().fg(self.theme.text)),
                        ]
                    })
                    .collect::<Vec<_>>(),
            );
        }
        // Built before the chain because it needs the workspace's own name and contents; the
        // other arms are constants.
        let armed_close_summary = self.close_workspace_armed.as_ref().map(|_| {
            let name = self.workspace().map(|w| w.name.as_str()).unwrap_or("this");
            let panes = self.workspace().map(|w| w.panes.len()).unwrap_or(0);
            let agents = self.running_agents_here();
            let agents = if agents == 1 {
                "1 running agent".to_owned()
            } else {
                format!("{agents} running agents")
            };
            let panes = if panes == 1 { "1 pane".to_owned() } else { format!("{panes} panes") };
            format!(
                "CLOSE \u{201c}{name}\u{201d} · {panes}, {agents} · Enter or ✓ closes · Esc or ✘ cancels"
            )
        }).unwrap_or_default();
        let summary = if self.help_open {
            "HELP · Esc or ? closes"
        } else if self.rename_form.is_some() {
            "RENAME · type a pane name · Enter saves · Esc cancels"
        } else if self.launch_form.is_some() {
            "LAUNCH · type to filter · Enter reviews · Esc cancels"
        } else if self.close_workspace_armed.is_some() {
            // The one destructive mode said nothing here, so the keys that answered it were
            // undiscoverable and what it would destroy went unsaid.
            &armed_close_summary
        } else {
            "keys go to the focused pane · Ctrl+B ? help"
        };
        Line::styled(summary.to_owned(), Style::default().fg(self.theme.muted))
    }

    fn render_header(&self, frame: &mut Frame, area: Rect) {
        let workspace = self.workspace().map(|w| w.name.as_str()).unwrap_or("empty");
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    " d·ock ",
                    Style::default()
                        .fg(self.theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" runtime · {workspace} · protocol v{PROTOCOL_VERSION}"),
                    Style::default().fg(self.theme.text),
                ),
            ]))
            .block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .border_style(Style::default().fg(self.theme.border)),
            ),
            area,
        );
    }

    /// The workspace strip: one tab per workspace, numbered by the digit that jumps to it.
    ///
    /// The strip scrolls rather than truncating, and follows the active tab: a workspace you
    /// have jumped to is always visible, together with its own rename and close affordances,
    /// which are the last thing that should fall off an edge. `‹` and `›` mark tabs hidden on
    /// each side and are clickable. The numbers stay meaningful because they are positions,
    /// not labels.
    fn render_tabs(&mut self, frame: &mut Frame, area: Rect) {
        self.tab_strip_area = Some(area);
        let workspace_count = self.layout.workspaces.len();
        if workspace_count == 0 {
            return;
        }

        // Every tab's rendered width, measured once so the clamp below and the layout further
        // down agree on what "fits" means. Only the active tab carries rename and close
        // affordances, which is why it alone gets the extra width; the trailing `+ 1` on every
        // tab is the one-column gap this loop always left between tabs.
        let armed = self.close_workspace_armed.clone();
        let labels: Vec<String> = self
            .layout
            .workspaces
            .iter()
            .enumerate()
            .map(|(index, workspace)| format!(" {} {} ", index + 1, workspace.name))
            .collect();
        let widths: Vec<u16> = labels
            .iter()
            .enumerate()
            .map(|(index, label)| {
                let mut width = label.chars().count() as u16 + 1;
                if index == self.workspace_index {
                    width += 3; // ✎
                    width += 3; // ✘ (or the armed cancel)
                    if armed.as_deref() == Some(self.layout.workspaces[index].workspace_id.as_str())
                    {
                        width += 3; // ✓ confirm, once armed
                    }
                }
                width
            })
            .collect();

        // Reserved so `‹`/`›` never shift the strip by a column as they appear or disappear —
        // tabs are laid out between these two columns whether or not the markers are drawn.
        let available = area.width.saturating_sub(2);

        // Bounds clamp: runs every frame, and only ever keeps `tab_scroll` inside range so a
        // resize or a closed workspace cannot strand it past the end. It never chases the active
        // tab — that would undo a wheel scroll on the very next frame. See
        // `tab_scroll_last_active` for the correction that does chase it, and why it is kept
        // separate from this one.
        self.tab_scroll = self.tab_scroll.min(workspace_count - 1);

        // Bring-into-view correction: fires only when the active workspace differs from the one
        // shown last render, i.e. only on a jump (digit, `Ctrl+B w`, a click, `,`/`.`). A wheel
        // scroll never changes `workspace_index`, so it never reaches this branch, which is what
        // lets a strip the user positioned by hand stay exactly where they left it.
        //
        // Accepted consequence: narrowing the terminal can carry the active tab out of view
        // without a jump to bring it back — re-snapping on every width change would mean
        // tracking the previous width for a case the next jump already resolves for free.
        if self.tab_scroll_last_active != self.workspace_index {
            if self.tab_scroll > self.workspace_index {
                self.tab_scroll = self.workspace_index;
            }
            while self.tab_scroll < self.workspace_index {
                let span: u16 = widths[self.tab_scroll..=self.workspace_index].iter().sum();
                if span <= available {
                    break;
                }
                self.tab_scroll += 1;
            }
            self.tab_scroll_last_active = self.workspace_index;
        }

        let left_edge = area.x.saturating_add(1);
        let right_edge = area.right().saturating_sub(1);
        let mut x = left_edge;
        let mut last_drawn = None;
        for (index, label) in labels.iter().enumerate().skip(self.tab_scroll) {
            let active = index == self.workspace_index;
            let label_width = label.chars().count() as u16;
            if x.saturating_add(label_width) > right_edge {
                break;
            }
            let workspace = &self.layout.workspaces[index];
            let tab = Rect::new(x, area.y, label_width, 1);
            let style = if active {
                Style::default()
                    .bg(self.theme.accent)
                    .fg(self.theme.surface)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(self.theme.muted)
            };
            frame.render_widget(Paragraph::new(Line::styled(label.clone(), style)), tab);
            self.tab_areas.push((workspace.workspace_id.clone(), tab));
            x = x.saturating_add(label_width);
            // The rename affordance rides only on the active tab. On every tab it would be
            // clutter, and on none of them renaming would stay keyboard-only.
            if active && x.saturating_add(3) <= right_edge {
                let pencil = Rect::new(x, area.y, 3, 1);
                frame.render_widget(Paragraph::new(Line::styled(" ✎ ", style)), pencil);
                self.rename_workspace_area = Some(pencil);
                x = x.saturating_add(3);
            }
            // Close rides beside rename, and asks before it acts. Closing a workspace takes
            // every pane in it and every process still running in them, which is far too much
            // to hang off one stray click on a three-cell target; the armed tab says so in
            // words rather than opening a ninth modal overlay.
            if active {
                let tab_armed = armed.as_deref() == Some(workspace.workspace_id.as_str());
                // Cancel keeps the cell the close control had, and confirm is the new target to
                // its right. Putting confirm first read better and was a trap: the second press
                // of a double-click would land on it and destroy the workspace the first press
                // had only asked about.
                if x.saturating_add(3) <= right_edge {
                    let cancel = Rect::new(x, area.y, 3, 1);
                    let cancel_style = if tab_armed {
                        Style::default()
                            .bg(self.theme.blocked)
                            .fg(self.theme.surface)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        style
                    };
                    frame.render_widget(Paragraph::new(Line::styled(" ✘ ", cancel_style)), cancel);
                    self.close_workspace_area = Some(cancel);
                    x = x.saturating_add(3);
                }
                if tab_armed && x.saturating_add(3) <= right_edge {
                    let confirm = Rect::new(x, area.y, 3, 1);
                    frame.render_widget(
                        Paragraph::new(Line::styled(
                            " ✓ ",
                            Style::default()
                                .bg(self.theme.blocked)
                                .fg(self.theme.surface)
                                .add_modifier(Modifier::BOLD),
                        )),
                        confirm,
                    );
                    self.confirm_close_workspace_area = Some(confirm);
                    x = x.saturating_add(3);
                }
            }
            x = x.saturating_add(1);
            last_drawn = Some(index);
        }
        if x.saturating_add(3) <= right_edge {
            let plus = Rect::new(x, area.y, 3, 1);
            frame.render_widget(
                Paragraph::new(Line::styled(" + ", Style::default().fg(self.theme.accent))),
                plus,
            );
            self.new_workspace_area = Some(plus);
        }

        if self.tab_scroll > 0 {
            let marker = Rect::new(area.x, area.y, 1, 1);
            frame.render_widget(
                Paragraph::new(Line::styled("‹", Style::default().fg(self.theme.accent))),
                marker,
            );
            self.tab_scroll_left_area = Some(marker);
        }
        let hides_tabs_on_the_right = match last_drawn {
            Some(last) => last + 1 < workspace_count,
            None => self.tab_scroll < workspace_count,
        };
        if hides_tabs_on_the_right {
            let marker = Rect::new(right_edge, area.y, 1, 1);
            frame.render_widget(
                Paragraph::new(Line::styled("›", Style::default().fg(self.theme.accent))),
                marker,
            );
            self.tab_scroll_right_area = Some(marker);
        }
    }

    /// The chooser overlay: a query line, then the matching rows, best first.
    fn render_picker(&mut self, frame: &mut Frame, area: Rect) {
        let Some((purpose, picker)) = self.picker.as_ref() else {
            return;
        };
        let title = match purpose {
            PickerPurpose::Workspace => " WORKSPACES ",
            PickerPurpose::File => " FILES ",
        };
        let width = area.width.min(58);
        let height = area.height.min(14);
        let popup = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        // The overlay floats above live panes, so its own background has to be painted or their
        // text shows through the gaps between its rows.
        frame.render_widget(Clear, popup);

        let rows = usize::from(height.saturating_sub(4));
        let mut lines = vec![
            Line::from(vec![
                Span::styled(
                    "› ",
                    Style::default()
                        .fg(self.theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{}█", picker.query()),
                    Style::default().fg(self.theme.text),
                ),
            ]),
            Line::from(""),
        ];
        if picker.is_empty() {
            // A picker has nothing to offer about an empty result but the fact of it. Writing a
            // task down from an empty query is the board overlay's job, and the board overlay
            // says so itself.
            lines.push(Line::styled(
                "  no match",
                Style::default().fg(self.theme.muted),
            ));
        }
        // The detail column is right-aligned against the popup's inner width so the counts form a
        // column rather than trailing each name at a different offset.
        let inner = usize::from(width.saturating_sub(4));
        // The first row sits below the border, the query line, and the blank after it.
        let mut row_y = popup.y.saturating_add(3);
        let mut row_areas = Vec::new();
        for (item, selected) in picker.visible().take(rows) {
            row_areas.push(Rect::new(
                popup.x.saturating_add(1),
                row_y,
                width.saturating_sub(2),
                1,
            ));
            row_y = row_y.saturating_add(1);
            let gap =
                inner.saturating_sub(item.label.chars().count() + item.detail.chars().count() + 2);
            lines.push(Line::from(vec![
                Span::styled(
                    if selected { "› " } else { "  " },
                    Style::default().fg(self.theme.accent),
                ),
                Span::styled(
                    item.label.clone(),
                    if selected {
                        Style::default()
                            .fg(self.theme.accent)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(self.theme.text)
                    },
                ),
                Span::raw(" ".repeat(gap)),
                Span::styled(item.detail.clone(), Style::default().fg(self.theme.muted)),
            ]));
        }
        self.picker_row_areas = row_areas;
        frame.render_widget(
            Paragraph::new(lines)
                .style(Style::default().fg(self.theme.text).bg(self.theme.surface))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(Theme::border_type())
                        .border_style(Style::default().fg(self.theme.border_focused))
                        .title(title),
                ),
            popup,
        );
    }

    fn render_sidebar(&mut self, frame: &mut Frame, area: Rect) {
        let heading = Style::default()
            .fg(self.theme.text)
            .add_modifier(Modifier::BOLD);
        // Rendered without wrapping, so one pushed line is exactly one rendered row and
        // `clickable_row` can address a row by its index in `lines`. Every line carrying
        // variable-length text is ellipsised to the width the right border leaves, because a
        // line that wrapped would slide every row beneath it out from under the rectangles
        // this records for the pointer.
        let inner_width = usize::from(area.width).saturating_sub(1);
        let mut rows = SidebarRows::new(area.height);
        rows.push(|| Line::styled("WORKSPACES", heading));
        for (index, workspace) in self.layout.workspaces.iter().enumerate() {
            rows.push(|| {
                Line::styled(
                    format!(
                        "{} {}",
                        if index == self.workspace_index {
                            "›"
                        } else {
                            " "
                        },
                        ellipsise(&workspace.name, inner_width.saturating_sub(2))
                    ),
                    Style::default().fg(if index == self.workspace_index {
                        self.theme.accent
                    } else {
                        self.theme.muted
                    }),
                )
            });
        }
        rows.push(|| Line::from(""));
        rows.push(|| Line::styled("AGENTS", heading));
        // Sized so the leading glyph and its two spaces still fit inside the border.
        let label_width = inner_width.saturating_sub(3);
        let roster = self.agent_roster();
        let roster_is_empty = roster.is_empty();
        for (state, label, task, workspace) in roster {
            // An agent below the sidebar's last row cannot be seen, and neither can any agent
            // after it. Everything below is off the bottom too, so every remaining index is
            // one `clickable_row` already answers `None` for and no rectangle can be misplaced
            // by stopping here. Formatting the rest was formatting for nobody: a busy runtime
            // spans workspaces, and this list used to build two lines per agent in all of them.
            if !rows.has_room() {
                break;
            }
            // The state is spelled out beside the name. The glyph and its colour say that
            // something is true of this agent; only the word says what, and "needs you" is the
            // whole reason to look at this list at all.
            let state_text = state.label();
            // The task rides with the name, because which agent is which is the question a roster
            // of three identical "claude" rows cannot answer.
            let named = match &task {
                Some(detail) => format!("{label} #{detail}"),
                None => label.to_owned(),
            };
            let name_width = label_width.saturating_sub(state_text.chars().count() + 2);
            // The workspace goes inline when it fits and on its own line when it does not. The
            // sidebar is narrow enough that appending it unconditionally ellipsised it away,
            // which reads as a roster answering "which workspace" and in fact never saying. A
            // second line costs a row and always says it.
            let inline = workspace
                .map(|workspace| format!("{named} · {workspace}"))
                .filter(|inline| inline.chars().count() <= name_width);
            let overflow = match (&inline, workspace) {
                (None, Some(workspace)) => Some(workspace),
                _ => None,
            };
            let named = inline.unwrap_or(named);
            let name = ellipsise(&named, name_width);
            let gap = label_width.saturating_sub(name.chars().count() + state_text.chars().count());
            rows.push(|| {
                Line::from(vec![
                    Span::styled(
                        format!(" {} {name}", state.glyph()),
                        Style::default().fg(self.theme.agent(state)),
                    ),
                    Span::raw(" ".repeat(gap.max(1))),
                    Span::styled(
                        state_text,
                        Style::default().fg(self.theme.agent(state)).add_modifier(
                            if state == AgentState::Blocked {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            },
                        ),
                    ),
                ])
            });
            if let Some(workspace) = overflow {
                // Indented under its agent and muted, so the eye reads it as belonging to the row
                // above rather than as another agent.
                rows.push(|| {
                    Line::styled(
                        format!(
                            "   in {}",
                            ellipsise(workspace, label_width.saturating_sub(4))
                        ),
                        Style::default().fg(self.theme.muted),
                    )
                });
            }
        }
        if roster_is_empty {
            rows.push(|| Line::styled(" none running", Style::default().fg(self.theme.muted)));
        }
        rows.push(|| Line::from(""));
        // What this pane of the sidebar used to hold was a list of agents running elsewhere on the
        // machine, which Dock has no way to control and which included the user's own editor
        // session. Its replacement answers the question a quiet dashboard actually raises: what
        // can I do from here. Each row is clickable, so the list is a menu rather than a poster.
        rows.push(|| Line::styled("START HERE", heading));
        self.quick_action_areas.clear();
        for (key, action, command) in QUICK_ACTIONS {
            rows.push(|| {
                Line::from(vec![
                    Span::styled(
                        format!(" {key} "),
                        Style::default()
                            .fg(self.theme.accent)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        ellipsise(action, inner_width.saturating_sub(key.chars().count() + 2)),
                        Style::default().fg(self.theme.text),
                    ),
                ])
            });
            if let Some(row) = clickable_row(area, rows.last()) {
                self.quick_action_areas.push((command, row));
            }
        }
        // Launch keeps its own emphatic row: it is the one action that starts work rather than
        // showing something, and it was the only discoverable action here before.
        rows.push(|| Line::from(""));
        rows.push(|| {
            Line::styled(
                "Ctrl+B l LAUNCH AGENT",
                Style::default()
                    .fg(self.theme.accent)
                    .add_modifier(Modifier::BOLD),
            )
        });
        self.launch_area = clickable_row(area, rows.last());
        frame.render_widget(
            Paragraph::new(rows.into_lines()).block(
                Block::default()
                    .borders(Borders::RIGHT)
                    .border_style(Style::default().fg(self.theme.border)),
            ),
            area,
        );
    }

    /// Live agents ordered by how much they are costing the user: blocked first, then by
    /// label so the list does not reshuffle between frames for equally urgent agents.
    ///
    /// Only a run whose agent was actually detected is an agent. `self.agents` also carries
    /// every pane's ambient shell, which reports no kind at all; listing those turned the
    /// roster into a list of run ids for processes that are not agents.
    ///
    /// The task and the workspace are borrowed rather than copied, so an entry the sidebar
    /// turns out to have no room for costs nothing beyond the comparison that sorted it. Only
    /// a task this dashboard dispatched itself has to be built, because that one is held as a
    /// number rather than as text.
    fn agent_roster(&self) -> Vec<RosterEntry<'_>> {
        // Indexed once, not asked once per agent. Answering "which workspace" for a single run
        // means walking every pane of every workspace, and answering "which task" means walking
        // the run list; asking them per agent made this list cost agents × workspaces × panes
        // on a path that runs on every frame.
        let workspaces = self.workspaces_by_run();
        let tasks = self.tasks_by_run();
        // Joined to the run so the roster can say which task each agent is on. Three agents all
        // reading "claude" tell you only that three agents are running.
        let mut roster: Vec<RosterEntry<'_>> = self
            .agents
            .iter()
            .filter_map(|(run_id, (kind, state))| {
                // The workspace rides alongside the task for the same reason the task rides
                // with the name: this list spans every workspace, so "needs you" is actionable
                // only once you know where to go.
                let task = tasks.get(run_id.as_str()).cloned();
                let workspace = workspaces.get(run_id.as_str()).copied();
                Some((*state, kind.as_ref()?.label(), task, workspace))
            })
            .collect();
        roster.sort_by(|left, right| {
            left.0
                .attention_rank()
                .cmp(&right.0.attention_rank())
                .then_with(|| left.1.cmp(right.1))
                .then_with(|| left.2.cmp(&right.2))
                .then_with(|| left.3.cmp(&right.3))
        });
        roster
    }

    /// The workspace each run is in, by the name a person gave it, in one pass over the layout.
    ///
    /// The roster is the one view that spans workspaces, so it was also the only place an agent
    /// could say it needs you without saying where to go and answer it.
    ///
    /// Read from the layout rather than the run list because the layout is what carries the name;
    /// a snapshot knows only the id, and an id is what the name exists to avoid. First workspace
    /// wins, because a run bound into two panes is a bug rather than two homes.
    fn workspaces_by_run(&self) -> HashMap<&str, &str> {
        let mut index = HashMap::new();
        for workspace in &self.layout.workspaces {
            for pane in workspace.panes.values() {
                if let Some(run_id) = pane.run_id.as_deref() {
                    index.entry(run_id).or_insert(workspace.name.as_str());
                }
            }
        }
        index
    }

    /// The task each run is on, in one pass, giving the same answer for every run that
    /// [`task_of`](Self::task_of) gives for one.
    ///
    /// The daemon's binding is the only source now: a dispatch records the card on the run
    /// itself, so this no longer needs a client-local note that a quit would take with it.
    fn tasks_by_run(&self) -> HashMap<&str, Cow<'_, str>> {
        self.runs
            .iter()
            .filter_map(|run| Some((run.run_id.as_str(), Cow::Borrowed(bound_task(run)?))))
            .collect()
    }

    /// The task a run is working on, according to the daemon that owns the run.
    ///
    /// There used to be a client-local fallback here, because an unbound launch had nowhere
    /// durable to record which card it was for. It went with the dashboard when it quit, and a
    /// second dashboard never had it at all; the request carries the reference now.
    fn task_of(&self, run_id: &str) -> Option<String> {
        self.runs
            .iter()
            .find(|run| run.run_id == run_id)
            .and_then(bound_task)
            .map(str::to_owned)
    }

    /// The runs lane: one row per live agent pane, in the order a person should look at them.
    ///
    /// Derived from data this client already holds and pushed to it — `self.runs` for where each
    /// run lives and what task it was bound to, `self.agents` for what it is doing, `self.queues`
    /// for what is waiting behind it — so the lane is live the moment it renders and asks the
    /// daemon for nothing of its own.
    ///
    /// A pane with no agent in it is not a row. `AgentState::Idle` does not mean "the agent is
    /// resting", it means no agent was detected in this pane at all, so a lane that admitted them
    /// would list every shell on the canvas under a state none of them is in.
    fn live_runs(&self) -> Vec<LiveRun<'_>> {
        let mut rows: Vec<LiveRun<'_>> = self
            .runs
            .iter()
            .filter_map(|run| {
                let (agent, state) = self.agents.get(run.run_id.as_str())?;
                // Keyed by the pane rather than the run, because that is how the daemon keys a
                // queue: a queue outlives the run it was filled for, and a pane that is
                // relaunched must not come back with somebody else's queue behind it.
                let queue = self.queue_for(run.workspace_id.as_str(), run.pane_id.as_str());
                Some(LiveRun {
                    run_id: run.run_id.as_str(),
                    workspace_id: run.workspace_id.as_str(),
                    pane_id: run.pane_id.as_str(),
                    agent: (*agent)?,
                    state: *state,
                    task_id: bound_task(run).and_then(|task| task.parse().ok()),
                    queued: queue.map_or(0, |queue| queue.entries.len()),
                    auto_feed: queue.is_some_and(|queue| queue.auto_feed),
                    awaiting_ack: queue.is_some_and(|queue| queue.awaiting_ack),
                    holding_because: queue.and_then(|queue| queue.holding_because.as_deref()),
                })
            })
            .collect();
        // Blocked agents first, for the reason the sidebar sorts that way: they are the only ones
        // costing the user throughput while they wait.
        rows.sort_by(|left, right| {
            left.state
                .attention_rank()
                .cmp(&right.state.attention_rank())
                .then_with(|| left.agent.label().cmp(right.agent.label()))
                .then_with(|| left.pane_id.cmp(right.pane_id))
        });
        rows
    }

    /// The daemon's queue for one pane, if it holds one.
    ///
    /// A linear scan on purpose. The daemon caps itself at `MAX_QUEUED_TOTAL` entries spread over
    /// a handful of panes, and the lane has one row per *agent*, so this is a few comparisons
    /// against a few strings — where a map keyed by the pair would allocate a `String` key per
    /// pane per frame to look itself up.
    fn queue_for(&self, workspace_id: &str, pane_id: &str) -> Option<&PaneQueueSnapshot> {
        self.queues
            .iter()
            .find(|queue| queue.workspace_id == workspace_id && queue.pane_id == pane_id)
    }

    /// Replaces the replicated queue listing with the daemon's.
    ///
    /// Deliberately does not touch `self.error`: this arrives on the back of every refresh,
    /// including the one that follows a refusal, and a listing that cleared the footer would
    /// erase the refusal before it had been read.
    pub fn set_queues(&mut self, queues: Vec<PaneQueueSnapshot>, paused: bool) {
        self.queues = queues;
        self.queues_paused = paused;
    }

    /// Takes the daemon's answer to a queue request.
    ///
    /// The refusal is the product here, so it is surfaced in the daemon's own words rather than
    /// being folded into a house message: arming a pane whose agent has never reported a state
    /// is answered with the sentence naming `dock hooks --install`, and a person who never sees
    /// that sentence is left with a queue that is silently never going to fire. The success case
    /// carries the whole listing, so the lane is already right when the frame after this paints.
    pub fn apply_queue_response(&mut self, response: Response) {
        match response {
            Response::Queues { queues, paused } => {
                self.set_queues(queues, paused);
                self.error = None;
            }
            Response::Error { message, .. } => self.error = Some(message),
            other => self.error = Some(format!("unexpected queue response: {other:?}")),
        }
    }

    /// The board the cursor is on: the overlay's when one is open, the pane's otherwise.
    ///
    /// One cursor over two surfaces, because they are two ways of looking at one board rather
    /// than two boards. Whichever is taking keys is the one the cursor resolves against, and
    /// both hold the same tasks — `set_board_tasks` fills them together.
    fn cursor_view(&self) -> Option<&BoardView> {
        self.board
            .as_ref()
            .map(|board| &board.view)
            .or(self.board_pane_view.as_ref())
    }

    /// Where the cursor is now, as a column and a position in it, resolved against what is on
    /// the board this frame rather than remembered as a pair of numbers.
    ///
    /// Falls back to the first entry when the thing the cursor named has left the board — a
    /// cursor still pointing at a departed pane would answer `a` by arming nothing and saying
    /// nothing. Before it has been moved at all the view's own opening position stands, so a
    /// board still opens on the leftmost column that has anything in it.
    fn board_cursor_at(&self, view: &BoardView, live: &BoardLive<'_>) -> (usize, usize) {
        let last = view.statuses().len().saturating_sub(1);
        let (column, wanted) = match self.board_cursor.as_ref() {
            Some((column, target)) => ((*column).min(last), target.as_ref()),
            None => (view.column(), None),
        };
        let targets = column_targets(view, live, column);
        let index = match wanted {
            Some(target) => targets
                .iter()
                .position(|entry| entry == target)
                .unwrap_or(0),
            None => view.row(),
        };
        (column, index.min(targets.len().saturating_sub(1)))
    }

    /// What the cursor would be on after a move, without touching anything.
    fn next_board_target(
        &self,
        columns: isize,
        rows: isize,
    ) -> Option<(usize, Option<BoardTarget>)> {
        let view = self.cursor_view()?;
        let runs = self.live_runs();
        let live = BoardLive::new(&runs);
        let (column, index) = self.board_cursor_at(view, &live);
        // Saturating rather than wrapping, for the reason the lane's cursor was: the key this
        // carries arms an agent, and wrapping off the last entry would put a held `j` on the
        // agent the user was moving away from.
        let column = column
            .saturating_add_signed(columns)
            .min(view.statuses().len().saturating_sub(1));
        let targets = column_targets(view, &live, column);
        // The index is kept across a column move and clamped there, the way `move_column` keeps
        // a row: a taller column to the left leaves it past the end of a shorter one.
        let index = index
            .saturating_add_signed(rows)
            .min(targets.len().saturating_sub(1));
        Some((column, targets.into_iter().nth(index)))
    }

    /// The board's cursor, moved by columns and by entries. Nothing here asks the daemon
    /// anything: the whole grid is already in this client's hands.
    fn move_board_cursor(&mut self, columns: isize, rows: isize) -> UiCommand {
        if let Some((column, target)) = self.next_board_target(columns, rows) {
            self.set_board_cursor(column, target);
        }
        UiCommand::None
    }

    /// Puts the cursor somewhere and takes both views with it.
    ///
    /// A card target is followed into each view, so everything that still reads a view's own
    /// cursor — `<` and `>` moving a card, the overlay opened over a pane — agrees with the one
    /// cursor instead of keeping a second opinion about where the user is.
    fn set_board_cursor(&mut self, column: usize, target: Option<BoardTarget>) {
        if let Some(BoardTarget::Card(id)) = target {
            if let Some(view) = self.board_pane_view.as_mut() {
                view.follow(id);
            }
            if let Some(board) = self.board.as_mut() {
                board.view.follow(id);
            }
        }
        self.board_cursor = Some((column, target));
    }

    /// The card the cursor is on, if it is on a card at all.
    fn cursor_card(&self) -> Option<u64> {
        let view = self.cursor_view()?;
        let runs = self.live_runs();
        let live = BoardLive::new(&runs);
        let (column, index) = self.board_cursor_at(view, &live);
        match column_targets(view, &live, column).into_iter().nth(index)? {
            BoardTarget::Card(id) => Some(id),
            BoardTarget::Pane(..) => None,
        }
    }

    /// The agent under the cursor, as everything `a` needs to act on it: where the cursor is,
    /// which pane is running it, and whether the daemon already has that pane armed.
    ///
    /// Owned rather than borrowed on purpose — the caller sets `self.error`, and the borrows
    /// this is assembled from all point back into `self`.
    fn cursor_agent(&self) -> Result<(usize, BoardTarget, String, String, bool), String> {
        let Some(view) = self.cursor_view() else {
            return Err("no board to arm anything on".into());
        };
        let runs = self.live_runs();
        let live = BoardLive::new(&runs);
        let (column, index) = self.board_cursor_at(view, &live);
        if view.statuses().get(column).map(String::as_str) != Some(ACTIVE_STATUS) {
            return Err("a arms an agent: move the cursor into ACTIVE first".into());
        }
        let entries = active_entries(view, &live);
        let Some(entry) = entries.get(index).copied() else {
            return Err("no agent to arm: nothing is running on this board".into());
        };
        let Some(run) = entry.run() else {
            return Err("that card has no agent running on it to arm".into());
        };
        Ok((
            column,
            entry.target(),
            run.workspace_id.to_owned(),
            run.pane_id.to_owned(),
            run.auto_feed,
        ))
    }

    /// Arms or disarms auto-feed for the pane under the cursor, and nothing else.
    ///
    /// One pane per press, named explicitly, and the toggle reads the pane's current state from
    /// the daemon's own listing rather than from anything this client decided — so `a` on an
    /// armed pane disarms it and `a` on a pane the daemon never armed asks for arming. There is
    /// deliberately no key that arms the column: arming is the one act that lets Dock type into
    /// an agent while nobody is watching, and "arm all" would make one keystroke's worth of
    /// deliberateness stand in for a whole canvas of it.
    fn toggle_auto_feed(&mut self) -> UiCommand {
        let (column, target, workspace_id, pane_id, armed) = match self.cursor_agent() {
            Ok(agent) => agent,
            Err(message) => {
                self.error = Some(message);
                return UiCommand::None;
            }
        };
        let request = Request::Queue(QueueRequest::SetAuto {
            workspace_id,
            pane_id,
            enabled: !armed,
        });
        // The cursor stays on the entry that was acted on, so the answer lands on what the user
        // was looking at even if `ACTIVE` re-sorts around it before the frame after.
        self.set_board_cursor(column, Some(target));
        self.error = None;
        UiCommand::Request(Box::new(request))
    }

    /// The keys a Board pane takes, which is the board's cursor and its one verb.
    ///
    /// This exists because a Board pane has no PTY and must never be given one. Ordinary panes
    /// reach the daemon through `send_to_pane`, which is unchanged and still drops input for a
    /// pane with no run; a board is intercepted before that, so nothing here can turn into
    /// `PaneInput` however the key is encoded. Anything unrecognised is ignored rather than
    /// guessed at — a board is not a keyboard surface, it has a cursor and a switch.
    fn board_pane_key(&mut self, key: KeyEvent) -> UiCommand {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.move_board_cursor(0, 1),
            KeyCode::Char('k') | KeyCode::Up => self.move_board_cursor(0, -1),
            KeyCode::Char('h') | KeyCode::Left => self.move_board_cursor(-1, 0),
            KeyCode::Char('l') | KeyCode::Right => self.move_board_cursor(1, 0),
            KeyCode::Char('a') => self.toggle_auto_feed(),
            _ => UiCommand::None,
        }
    }

    /// A Board pane: one grid, and the live agents drawn in the column they belong in.
    ///
    /// There used to be a runs lane stacked above the columns, on the theory that a run is not a
    /// status. It is not — but the first person to open this pane watched their own agent
    /// running in that strip and asked why their work "was not in the table". `ACTIVE` is the
    /// `in-progress` column with the live entries derived on top of it, which answers that
    /// without ever putting a card in two columns at once.
    fn render_board_pane(&self, frame: &mut Frame, inner: Rect, focused: bool) {
        if inner.height < 4 || inner.width < 12 {
            return;
        }
        let Some(view) = self.board_pane_view.as_ref() else {
            frame.render_widget(
                Paragraph::new(Line::styled(
                    "reading the board…",
                    Style::default().fg(self.theme.muted),
                )),
                inner,
            );
            return;
        };
        // Assembled once and used for every answer below. Building this is a pass over every run
        // on the canvas, and this pane used to do it twice per frame to ask two questions of the
        // same list.
        let runs = self.live_runs();
        let live = BoardLive::new(&runs);
        // Resolved once and handed to both the grid and the footer. Resolving it is a pass over
        // a column's entries, and the two of them asking separately made that two passes for one
        // answer that cannot have changed in between.
        let cursor = self.board_cursor_at(view, &live);
        render_board_columns(
            frame,
            &self.theme,
            view,
            Rect::new(
                inner.x,
                inner.y,
                inner.width,
                inner.height.saturating_sub(1),
            ),
            &live,
            cursor,
        );

        // One row, as wide as the pane, under a grid whose cards are thirty cells wide. It is
        // the only place the daemon-wide kill switch and a stalled queue's own sentence fit, and
        // it is the pane's only chrome: a board pane has no footer of its own the way the
        // overlay does.
        let mut footer = board_pane_footer(view, &live, cursor, self.queues_paused, focused);
        ellipsise_in_place(&mut footer, usize::from(inner.width));
        frame.render_widget(
            Paragraph::new(Line::styled(
                footer,
                Style::default().fg(if self.queues_paused {
                    self.theme.blocked
                } else {
                    self.theme.muted
                }),
            )),
            Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1),
        );
    }

    fn render_node(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        workspace: &WorkspaceLayout,
        node: &LayoutNode,
    ) {
        match node {
            LayoutNode::Pane { pane_id } => {
                self.pane_areas.insert(pane_id.clone(), area);
                let pane = &workspace.panes[pane_id];
                let focused = workspace.focused_pane_id == *pane_id;
                // Borrowed from the workspace rather than copied out of it. The workspace is the
                // caller's own, not `self`'s, so nothing here needs to own a run id or a pane
                // name — and both used to be freshly allocated once per pane on every frame.
                let run_id = pane.run_id.as_deref();
                let (agent, state) = run_id
                    .and_then(|id| self.agents.get(id).copied())
                    .unwrap_or((None, AgentState::Idle));
                let label: &str = agent.map_or(pane.name.as_str(), |kind| kind.label());
                // A pane whose process is gone keeps painting its last frame forever. Without
                // this the only difference between a live shell and a dead one is that typing
                // stops working, so the title has to carry the news and the recovery key.
                let exited = pane.runtime == PaneRuntime::Exited;
                let title = if pane.is_board() {
                    // No run, so no state glyph and no location: a board is not somewhere a
                    // process is, and a title that said "unbound" would be answering a question
                    // nobody asked about a pane that will never have a run.
                    " ▤ board ".to_owned()
                } else if exited {
                    format!(" ✗ {label} · exited · Ctrl+B R restarts ")
                } else {
                    format!(
                        " {} {} · {} ",
                        state.glyph(),
                        label,
                        self.pane_location(pane)
                    )
                };
                let title_colour = if exited {
                    self.theme.blocked
                } else {
                    self.theme.agent(state)
                };
                // Copy mode swallows every key for this pane, so the pane itself has to say
                // so. A flag rather than the mode itself: the render below needs `self`
                // mutably for the resize bookkeeping, and `CopyMode` owns a whole cloned
                // screen, so borrowing it across that — or, worse, cloning it — is not free.
                let copying =
                    run_id.is_some_and(|id| self.copy.as_ref().is_some_and(|mode| mode.is_for(id)));
                // A pane that has stopped following live output looks exactly like a pane whose
                // agent has hung. It has to say which it is, and say how to undo it: someone who
                // scrolled by accident with the wheel has no other way to find out. The title is
                // shortened to make room for it rather than the other way around — `fit_scroll_marker`
                // reserves room for the marker first and only ellipsises the title into what is
                // left, because the two are painted as independent titles on the same border row
                // and neither knows the other is there; without a shared reservation the one
                // rendered last (the marker) simply paints over the other.
                let scroll_offset = run_id
                    .and_then(|id| self.screens.get(id))
                    .map(PaneScreen::scroll_offset)
                    .filter(|offset| *offset > 0);
                let copy_prefix_width = if copying { " COPY".chars().count() } else { 0 };
                let title_row_budget = usize::from(area.width)
                    .saturating_sub(2) // borders
                    .saturating_sub(copy_prefix_width);
                let (title, scroll_marker) = match scroll_offset {
                    Some(offset) => fit_scroll_marker(&title, title_row_budget, offset),
                    None => (title, None),
                };
                // `title` already opens with a space, so the prefix needs none of its own.
                let title = if copying {
                    Line::from(vec![
                        Span::styled(
                            " COPY",
                            Style::default()
                                .fg(self.theme.accent)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(title),
                    ])
                } else {
                    Line::from(title)
                };
                // Measured before the block takes ownership: the controls sit on the same border
                // row as the title, so they are only drawn when they cannot land on top of it.
                // The exited title carries the key that brings the pane back, and burying that
                // under a close button would be the worst possible trade.
                // Split and close ride on the focused pane's own border, on the right where a
                // window's controls live. Rendered as a right-aligned title so ratatui lays them
                // out rather than being painted over the border afterwards, which is how they
                // first landed on top of the exited pane's recovery hint.
                // An exited pane cannot be split: asking a dead pane to divide itself is a
                // strange thing to offer, and its title is carrying the key that brings it back,
                // so the controls take as little of that row as they can. Rename survives the
                // exit because naming a pane is how a person keeps track of what it was for,
                // which matters most for the one that stopped.
                const LIVE_CONTROLS: [(PaneControl, &str); 4] = [
                    (PaneControl::SplitHorizontal, " ⇋ "),
                    (PaneControl::SplitVertical, " ⇵ "),
                    (PaneControl::Rename, " ✎ "),
                    (PaneControl::Close, " × "),
                ];
                const EXITED_CONTROLS: [(PaneControl, &str); 2] =
                    [(PaneControl::Rename, " ✎ "), (PaneControl::Close, " × ")];
                let controls: &[(PaneControl, &str)] = if exited {
                    &EXITED_CONTROLS
                } else {
                    &LIVE_CONTROLS
                };
                let controls_width = 3 * controls.len() as u16;
                // On the bottom border, not the top. The top border is already carrying the pane's
                // identity and, when it has exited, the key that brings it back — and the first
                // attempt at this truncated that hint to "Ctrl+B R resta ×". The bottom border is
                // empty on every pane, so the controls cost nothing to put there.
                let show_controls = focused && area.width >= controls_width + 4;
                let block = Block::default()
                    .borders(Borders::ALL)
                    .border_type(Theme::border_type())
                    .title(title)
                    .title_style(Style::default().fg(title_colour))
                    .border_style(Style::default().fg(if focused {
                        self.theme.border_focused
                    } else {
                        self.theme.border
                    }));
                let block = if show_controls {
                    block.title_bottom(
                        Line::from(
                            controls
                                .iter()
                                .map(|(control, glyph)| {
                                    Span::styled(
                                        *glyph,
                                        Style::default().fg(if *control == PaneControl::Close {
                                            self.theme.blocked
                                        } else {
                                            self.theme.muted
                                        }),
                                    )
                                })
                                .collect::<Vec<_>>(),
                        )
                        .right_aligned(),
                    )
                } else {
                    block
                };
                let block = match &scroll_marker {
                    Some(note) => block.title_top(
                        Line::styled(note.clone(), Style::default().fg(self.theme.muted))
                            .right_aligned(),
                    ),
                    None => block,
                };
                let inner = block.inner(area);
                self.queue_resize(&workspace.workspace_id, pane_id, run_id, inner);
                frame.render_widget(block, area);
                self.pane_inner_areas.insert(pane_id.clone(), inner);
                if show_controls {
                    // The rectangles are derived from the right edge rather than measured from
                    // the render, because the title is laid out by ratatui and never reports back
                    // where it put anything.
                    let mut x = area.right().saturating_sub(1 + controls_width);
                    let row = area.bottom().saturating_sub(1);
                    for (control, _) in controls {
                        self.pane_control_areas
                            .push((*control, Rect::new(x, row, 3, 1)));
                        x = x.saturating_add(3);
                    }
                }
                // Where a pane's kind actually decides something. Everything above — the
                // rectangle, the focus, the border, the controls — is identical for both kinds,
                // which is exactly why the kind is a field on the pane rather than a variant in
                // the layout tree.
                if pane.is_board() {
                    self.render_board_pane(frame, inner, focused);
                    return;
                }
                // A frozen pane is painted from its own clone of the screen, which is what
                // makes copy mode a freeze rather than a claim: the live parser keeps
                // consuming every pushed delta behind it, and none of them reach the grid the
                // selection was made against.
                let copying =
                    run_id.and_then(|id| self.copy.as_ref().filter(|mode| mode.is_for(id)));
                let painted = match &copying {
                    Some(mode) => Some(mode.frozen.screen()),
                    None => run_id
                        .and_then(|id| self.screens.get(id))
                        .map(PaneScreen::screen),
                };
                match painted {
                    Some(screen) => {
                        // The cursor belongs to whichever pane is taking keystrokes; drawing
                        // one in every pane would make focus unreadable. In copy mode the
                        // PTY's own cursor is hidden too: the copy cursor is the one that
                        // moves, and two blocks would make it ambiguous which is which.
                        let mut cursor = Cursor::default();
                        if !focused || copying.is_some() {
                            cursor.hide();
                        }
                        frame.render_widget(PseudoTerminal::new(screen).cursor(cursor), inner);
                        if let Some(mode) = copying {
                            self.render_copy_overlay(frame, inner, &mode.session);
                        }
                    }
                    None => {
                        let mut placeholder = vec![Line::styled(
                            "starting…",
                            Style::default().fg(self.theme.muted),
                        )];
                        if run_id.is_none() {
                            placeholder.push(Line::styled(
                                "Ctrl+B R starts a shell · Ctrl+B l launches an agent here",
                                Style::default().fg(self.theme.muted),
                            ));
                        }
                        frame.render_widget(
                            Paragraph::new(placeholder).wrap(Wrap { trim: true }),
                            inner,
                        );
                    }
                }
            }
            LayoutNode::Split {
                axis,
                ratio_milli,
                first,
                second,
            } => {
                let (a, divider, b) = split_rect(area, *axis, *ratio_milli);
                let resize_pane = first_leaf(second).to_owned();
                self.dividers.push(Divider {
                    area: divider,
                    pane_id: resize_pane,
                    axis: *axis,
                    container: area,
                });
                self.render_node(frame, a, workspace, first);
                self.render_node(frame, b, workspace, second);
            }
        }
    }

    /// Paints the selection run and the copy cursor over the emulated screen.
    ///
    /// Only backgrounds are touched: `PseudoTerminal` stays the single thing that draws the
    /// text, so a highlight can never disagree with what the pane actually shows.
    fn render_copy_overlay(&self, frame: &mut Frame, inner: Rect, session: &CopySession) {
        let buffer = frame.buffer_mut();
        if let Some((from, to)) = session.selection() {
            let (start, end) = if from <= to { (from, to) } else { (to, from) };
            for row in start.0..=end.0 {
                // Reading order, not a rectangle: whole rows in the middle and only the
                // anchored halves of the first and last. This is the same run
                // `VtTerminal::selection_text` extracts, so the highlight previews the yank
                // rather than offering a second opinion about it.
                //
                // True only because the two conventions were deliberately aligned by
                // whole-branch review C1: `last` is INCLUSIVE here, and `selection_text` now
                // advances its end column by one to match, since the `vt100` call underneath
                // it is column-exclusive. Until then the claim above was false and the
                // clipboard was silently one character short of the highlight on every
                // selection. `a_mid_row_selection_yanks_exactly_as_many_characters_as_it_highlights`
                // guards the agreement so the two halves cannot drift apart again.
                let first = if row == start.0 { start.1 } else { 0 };
                let last = if row == end.0 {
                    end.1
                } else {
                    inner.width.saturating_sub(1)
                };
                for column in first..=last {
                    if let Some(cell) = cell_at(buffer, inner, row, column) {
                        cell.set_bg(self.theme.selection);
                    }
                }
            }
        }
        let (row, column) = session.cursor();
        if let Some(cell) = cell_at(buffer, inner, row, column) {
            cell.set_bg(self.theme.accent).set_fg(self.theme.surface);
        }
    }

    /// The binding facts the pane body used to spell out line by line. They survive as a
    /// title suffix because the body is now the emulated screen and has no room for them.
    fn pane_location(&self, pane: &PaneLayout) -> String {
        let Some(run_id) = pane.run_id.as_deref() else {
            return "unbound".into();
        };
        let Some(_run) = self.runs.iter().find(|run| run.run_id == run_id) else {
            return format!("{run_id} · unavailable");
        };
        // The task first when there is one: a run id identifies a row in a receipt, a task
        // identifies the work, and only one of those is what a person is looking for.
        match self.task_of(run_id) {
            Some(task) => format!("#{task} · {run_id}"),
            None => run_id.to_owned(),
        }
    }

    /// Announces a pane's inner geometry to the daemon, but only when it actually changed.
    /// `render` runs on every frame, so re-sending an identical size would put one resize
    /// request per pane per frame onto a socket that is otherwise silent when nothing moves.
    ///
    /// The record is keyed by run as well as size: a pane that just gained a run must be
    /// announced even at unchanged geometry, because the new PTY started at the daemon's
    /// own default rather than at this pane's size.
    fn queue_resize(
        &mut self,
        workspace_id: &str,
        pane_id: &str,
        run_id: Option<&str>,
        inner: Rect,
    ) {
        let Some(run_id) = run_id else {
            return;
        };
        let geometry = (run_id.to_owned(), inner.height, inner.width);
        if self.pane_geometry.get(pane_id) == Some(&geometry) {
            return;
        }
        self.pane_geometry.insert(pane_id.to_owned(), geometry);
        self.pending_resizes.push((
            workspace_id.to_owned(),
            pane_id.to_owned(),
            inner.height,
            inner.width,
        ));
    }

    fn render_narrow(&self, frame: &mut Frame, area: Rect) {
        let mut lines = vec![Line::styled(
            "d·ock · compact runtime",
            Style::default().fg(self.theme.accent),
        )];
        if let Some(workspace) = self.workspace() {
            lines.push(Line::from(format!(
                "{} · {} panes",
                workspace.name,
                workspace.panes.len()
            )));
            if let Some(pane) = workspace.panes.get(&workspace.focused_pane_id) {
                lines.push(Line::styled(
                    format!(
                        "› {} · {} · focused",
                        pane.name,
                        runtime_label(pane.runtime)
                    ),
                    Style::default().fg(self.theme.accent),
                ));
            }
        } else {
            lines.push(Line::from("No workspace · n create"));
        }
        lines.push(Line::styled(
            "Ctrl+B then n new · h/v split · Tab focus · l launch · ? help · q quit",
            Style::default().fg(self.theme.muted),
        ));
        frame.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: true }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(Theme::border_type())
                    .border_style(Style::default().fg(self.theme.border)),
            ),
            area,
        );
    }

    fn render_help(&self, frame: &mut Frame, area: Rect) {
        let width = area.width.min(72);
        let heading = Style::default()
            .fg(self.theme.accent)
            .add_modifier(Modifier::BOLD);
        let lines = vec![
            Line::styled("TYPING", heading),
            Line::from("Every key goes to the focused pane, Esc and Ctrl-C included."),
            Line::from("Ctrl+B is the only key Dock keeps; Ctrl+B Ctrl+B sends a literal one."),
            Line::styled("AFTER Ctrl+B", heading),
            Line::from("n new workspace   h/v split   z zoom"),
            Line::from("r rename   R restart shell   x close   l launch   q quit"),
            Line::from("w pick a workspace by name   1-9 jump to one   ,/. previous/next"),
            Line::from("f find a file here and type its path into the pane"),
            Line::from("a resume the agent that last ran here, continuing its own session"),
            Line::from("i review handoffs agents are waiting on: a accept · c request changes"),
            Line::from("k board: ←/→ column · ↑/↓ card · </> move · n new · Enter dispatch"),
            Line::from("B splits the same board into the canvas as a pane, with its runs lane"),
            Line::from("g what changed in this pane's worktree · j/k scroll · g/G ends"),
            Line::from("[ copy mode: hjkl move   v select   y yank   / search   Esc exits"),
            Line::from("d leaves the dashboard; runs keep running until you close them."),
            Line::from("Tab/S-Tab or arrows focus   +/- resize"),
            Line::from("PageUp/PageDown scroll history   End back to live output"),
            Line::styled("POINTER", heading),
            Line::from("Tab strip: click a tab to switch   ✎ rename   × close (twice)"),
            Line::from("+ at the end of the strip makes a workspace"),
            Line::from("Focused pane's lower border: ⇋ ⇵ split   ✎ rename   × close"),
            Line::from("Drag a divider to resize   drag inside a pane to select text"),
            Line::styled("FORMS AND PICKERS", heading),
            Line::from("type to filter/edit   ↑/↓ or j/k select   Enter review/confirm"),
            Line::from("Esc cancels a form rather than reaching the pane while one is open."),
            Line::styled("CURRENT", heading),
            Line::from(if self.workspace().is_some() {
                "Workspace selected; pane commands are available."
            } else {
                "No workspace: create one with Ctrl+B n before pane actions."
            }),
            Line::from("Esc or ? closes help"),
        ];
        // Sized to what it says. This list grows every time a control is published, and a fixed
        // height would keep the newest lines — the ones nobody knows about yet — off the bottom
        // of the one screen whose whole job is to publish them.
        let inner_width = usize::from(width.saturating_sub(2)).max(1);
        let rows: usize = lines
            .iter()
            .map(|line| line.width().div_ceil(inner_width).max(1))
            .sum();
        let height = area.height.min(rows as u16 + 2);
        let popup = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: true })
                .style(Style::default().fg(self.theme.text))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(Theme::border_type())
                        .border_style(Style::default().fg(self.theme.border_focused))
                        .title(" KEYMAP "),
                ),
            popup,
        );
    }

    fn render_rename(&self, frame: &mut Frame, area: Rect) {
        let width = area.width.min(48);
        let popup = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + area.height.saturating_sub(5) / 2,
            width,
            5,
        );
        let (target, value) = match self.rename_form.as_ref() {
            Some((target, value)) => (*target, value.as_str()),
            None => (RenameTarget::Pane, ""),
        };
        // The form says what it is renaming: the same box now reaches panes and workspaces, and
        // a rename that lands on the wrong one is invisible until something else looks wrong.
        let subject = match target {
            RenameTarget::Pane => "Pane",
            RenameTarget::Workspace => "Workspace",
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(format!("{subject} name: {value}█")),
                Line::from("Enter saves · Esc cancels"),
            ])
            .style(Style::default().fg(self.theme.text))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(Theme::border_type())
                    .border_style(Style::default().fg(self.theme.border_focused))
                    .title(" RENAME FOCUSED PANE "),
            ),
            popup,
        );
    }

    pub fn key(&mut self, key: KeyEvent) -> UiCommand {
        // The first open overlay in `OVERLAY_ORDER` takes the key, and the same array decides
        // what is drawn — so the two can no longer be edited apart.
        //
        // The whole stack sits ahead of the keymap on purpose: an open picker is taking a query
        // and copy mode owns every motion (`h`, `j`, `k`, `l`) and verb (`v`, `y`), so neither
        // can be allowed to reach a binding or the PTY as ordinary input.
        let overlay = self.open_overlays().next();
        if let Some(kind) = overlay {
            return self.overlay_key(kind, key);
        }
        // Esc answers the armed workspace close before it reaches the pane. The confirmation is
        // a question the dashboard put on screen, and the key that dismisses every other thing
        // Dock asks has to dismiss this one too rather than leaving it primed.
        // Enter answers the armed close, so the question is answerable without the mouse that
        // asked it. Ahead of the keymap because while a destructive question is on screen it is
        // the only thing Enter can sensibly mean.
        if self.close_workspace_armed.is_some() && key.code == KeyCode::Enter {
            return self.confirm_close_workspace();
        }
        if self.close_workspace_armed.is_some() && key.code == KeyCode::Esc {
            self.disarm_workspace_close();
            return UiCommand::None;
        }
        let encoding = self.encoding_for_focused_pane();
        match self.keymap.handle(key, encoding) {
            // Deliberately not a `Request`: pane input is fire-and-forget, and routing it
            // through the request arm would put two daemon round trips in front of the echo.
            // Dropped outright when the pane has no run: there is no PTY to receive it, and
            // sending anyway earns one daemon error per character straight into the footer.
            //
            // A focused Board pane takes the key here instead, ahead of `send_to_pane` and with
            // the encoded bytes thrown away. It is the one pane kind with a cursor of its own and
            // no process to type into, and routing it this way is what keeps `send_to_pane` and
            // `pane_input` untouched: a board never reaches either, so neither has to learn what
            // a board is. The prefix is already spent by `keymap.handle`, so `Ctrl+B` still
            // commands the dashboard from inside a board exactly as it does from a terminal.
            KeyOutcome::Passthrough(bytes) => {
                if self.focused_pane().is_some_and(PaneLayout::is_board) {
                    return self.board_pane_key(key);
                }
                self.send_to_pane(bytes)
            }
            KeyOutcome::Command(command) => self.run_command(command),
            KeyOutcome::PendingPrefix | KeyOutcome::Ignored => UiCommand::None,
        }
    }

    fn run_command(&mut self, command: PaneCommand) -> UiCommand {
        match command {
            PaneCommand::NewWorkspace => {
                let workspace_id = self.next_unique_id("workspace");
                let pane_id = self.next_unique_id("pane");
                UiCommand::Request(Box::new(Request::Workspace(WorkspaceRequest::Create {
                    name: workspace_id.replace('_', " "),
                    workspace_id,
                    pane_id,
                })))
            }
            PaneCommand::Split(axis) => self.split(axis, PaneKind::Terminal),
            // Focus is still ordinal rather than geometric, so the two backwards directions
            // and the two forwards ones collapse onto the existing cycle.
            PaneCommand::Focus(direction) => self.focus_next(matches!(
                direction,
                FocusDirection::Previous | FocusDirection::Left | FocusDirection::Up
            )),
            PaneCommand::Workspace(delta) => self.select_workspace(delta),
            PaneCommand::WorkspacePicker => self.open_workspace_picker(),
            PaneCommand::FilePicker => self.open_file_picker(),
            PaneCommand::ResumeAgent => self.resume_agent(),
            PaneCommand::Review => {
                self.error = None;
                UiCommand::LoadReviewInbox
            }
            PaneCommand::Board => {
                self.error = None;
                UiCommand::LoadBoard
            }
            // Horizontal, which divides by height here: a board is five columns wide before it is
            // anything else, and half the width of a split screen is not enough for five of them.
            PaneCommand::SplitBoard => self.split(SplitAxis::Horizontal, PaneKind::Board),
            PaneCommand::Git => {
                self.error = None;
                UiCommand::LoadGit
            }
            PaneCommand::WorkspaceJump(position) => self.jump_to_workspace(position),
            PaneCommand::Resize(delta) => self.resize_keyboard(delta),
            PaneCommand::Zoom => self.zoom(),
            PaneCommand::Rename => self.rename(),
            PaneCommand::Close => self.close(),
            PaneCommand::CloseWorkspace => self.close_workspace(),
            PaneCommand::Respawn => self.respawn(),
            PaneCommand::Launch => {
                self.open_launch();
                UiCommand::LoadCatalog
            }
            PaneCommand::ScrollPageUp => self.scroll_page_back(),
            PaneCommand::ScrollPageDown => self.scroll_page_forward(),
            PaneCommand::ScrollToLive => self.scroll_to_live(),
            PaneCommand::CopyMode => self.enter_copy_mode(),
            // The daemon owns every run, so leaving the dashboard signals nothing and tears
            // nothing down. Detaching and quitting are therefore the same act here.
            PaneCommand::Detach | PaneCommand::Quit => UiCommand::Quit,
            PaneCommand::Help => {
                self.error = None;
                self.help_open = true;
                UiCommand::None
            }
        }
    }

    /// Opens the workspace chooser. Cycling is fine for two workspaces and miserable for eight;
    /// this is how a distant one is reached without walking past every workspace in between.
    fn open_workspace_picker(&mut self) -> UiCommand {
        if self.layout.workspaces.is_empty() {
            self.error = Some("workspace unavailable: create a workspace first".into());
            return UiCommand::None;
        }
        let items = self
            .layout
            .workspaces
            .iter()
            .enumerate()
            .map(|(index, workspace)| PickerItem {
                key: workspace.workspace_id.clone(),
                label: workspace.name.clone(),
                detail: format!("{}  ·  {}", index + 1, pane_count(workspace.panes.len())),
            })
            .collect();
        self.picker = Some((PickerPurpose::Workspace, Picker::new(items)));
        self.error = None;
        UiCommand::None
    }

    /// Receives what Git says about the focused pane's worktree and opens the overlay over it.
    pub fn set_git(&mut self, facts: GitFacts, diff: String) {
        self.git = Some(GitOverlay {
            facts,
            diff: diff.lines().map(str::to_owned).collect(),
            scroll: 0,
        });
        self.error = None;
    }

    fn git_key(&mut self, key: KeyEvent) -> UiCommand {
        let Some(git) = self.git.as_mut() else {
            return UiCommand::None;
        };
        // Paging is bounded by the diff itself rather than by the visible height, which the key
        // handler does not know: scrolling past the end would show a blank overlay and read as a
        // broken render rather than as the end of the diff.
        let last = git.diff.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.git = None,
            KeyCode::Char('j') | KeyCode::Down => git.scroll = (git.scroll + 1).min(last),
            KeyCode::Char('k') | KeyCode::Up => git.scroll = git.scroll.saturating_sub(1),
            KeyCode::PageDown => git.scroll = (git.scroll + 10).min(last),
            KeyCode::PageUp => git.scroll = git.scroll.saturating_sub(10),
            KeyCode::Char('g') => git.scroll = 0,
            KeyCode::Char('G') => git.scroll = last,
            _ => {}
        }
        UiCommand::None
    }

    /// What has changed in the focused pane's worktree, painted with Dock's own palette.
    ///
    /// `delta` is not used here even when it is installed: it emits ANSI, and the overlay would
    /// have to un-escape that text only to style it again.
    /// The board: one column per status, cards inside them.
    ///
    /// Drawn as columns rather than a filtered list because the shape is the information — where
    /// the work has piled up, what is in flight, what is waiting on a person — and a list of the
    /// same tasks sorted by status shows none of it at a glance.
    fn render_board(&mut self, frame: &mut Frame, area: Rect) {
        let Some(board) = self.board.as_ref() else {
            return;
        };
        // Assembled before the popup is sized, because `ACTIVE` is one of the columns being
        // measured and how tall it is depends on what is running.
        let runs = self.live_runs();
        let live = BoardLive::new(&runs);
        // Sized to the tallest column rather than filling the screen: a board with four cards on
        // it should look like a board with four cards on it, not like one that has lost the rest.
        // `ACTIVE` counts double and can hold more entries than it has cards, so it is measured
        // in rows rather than in cards — sizing this to the card count alone clipped the bottom
        // of the one column that grew.
        let tallest = board
            .view
            .statuses()
            .iter()
            .map(|status| {
                if status == ACTIVE_STATUS {
                    active_entries(&board.view, &live).len() * 2
                } else {
                    board.view.cards(status).len()
                }
            })
            .max()
            .unwrap_or(0);
        let chrome = 7;
        let width = area.width.min(120);
        // Two rows are left for the dashboard's own footer, which is painted after every overlay
        // and would otherwise write over the bottom of this one.
        let height = (tallest as u16 + chrome)
            .max(10)
            .min(area.height.saturating_sub(3));
        let popup = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height.saturating_sub(2) - height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .border_type(Theme::border_type())
                .border_style(Style::default().fg(self.theme.border_focused))
                .style(Style::default().bg(self.theme.surface))
                .title(" BOARD "),
            popup,
        );
        let inner = Rect::new(
            popup.x + 1,
            popup.y + 1,
            popup.width.saturating_sub(2),
            popup.height.saturating_sub(2),
        );
        if inner.width < 20 || inner.height < 6 {
            return;
        }

        // The cards are drawn by the same function the Board pane uses, over the rectangle the
        // footer below does not want. There is no second renderer and no second data path: a
        // pane and an overlay differ in the rectangle they are handed and in whether Esc closes
        // them, which is the whole reason keeping the overlay costs nothing. They share the
        // cursor too — one board, one place the user is on it.
        render_board_columns(
            frame,
            &self.theme,
            &board.view,
            Rect::new(
                inner.x,
                inner.y,
                inner.width,
                inner.height.saturating_sub(2),
            ),
            &live,
            self.board_cursor_at(&board.view, &live),
        );

        // The footer carries the board's identity and its controls, which is where a person looks
        // when they do not already know what a key does.
        let footer = inner.bottom().saturating_sub(2);
        let hint = match board.composing.as_ref() {
            Some(title) => Line::from(vec![
                Span::styled(
                    "new task: ",
                    Style::default()
                        .fg(self.theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("{title}█"), Style::default().fg(self.theme.text)),
                Span::styled(
                    "   Enter adds it · Esc cancels",
                    Style::default().fg(self.theme.muted),
                ),
            ]),
            // The agent is named, because a dispatch that silently picks one is how every task on
            // this board ended up in-progress with a test stub behind it.
            None if board.writable => Line::from(vec![
                Span::styled(
                    "←/→ column · ↑/↓ card · </> move it · n new · Esc close · Enter → ",
                    Style::default().fg(self.theme.muted),
                ),
                match self.dispatch_adapter() {
                    Some(adapter) => Span::styled(
                        adapter.label(),
                        Style::default()
                            .fg(self.theme.accent)
                            .add_modifier(Modifier::BOLD),
                    ),
                    None => Span::styled(
                        "no agent installed",
                        Style::default().fg(self.theme.blocked),
                    ),
                },
            ]),
            None => Line::styled(
                "←/→ column · ↑/↓ card · Enter put an agent on it · Esc close · kanban-md owns this board",
                Style::default().fg(self.theme.muted),
            ),
        };
        frame.render_widget(hint, Rect::new(inner.x, footer, inner.width, 1));
        frame.render_widget(
            Paragraph::new(Line::styled(
                ellipsise(
                    &board.directory.display().to_string(),
                    usize::from(inner.width),
                ),
                Style::default().fg(self.theme.border),
            )),
            Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1),
        );
    }

    fn render_git(&self, frame: &mut Frame, area: Rect) {
        let Some(git) = self.git.as_ref() else {
            return;
        };
        let width = area.width.min(96);
        let height = area.height.min(26);
        let popup = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, popup);
        let heading = Style::default()
            .fg(self.theme.accent)
            .add_modifier(Modifier::BOLD);
        let muted = Style::default().fg(self.theme.muted);

        let facts = &git.facts;
        let mut lines = vec![
            Line::from(vec![
                Span::styled(facts.branch.clone(), heading),
                Span::styled(
                    format!(
                        "   {} files  +{} −{}   {} uncommitted",
                        facts.changed_files,
                        facts.insertions,
                        facts.deletions,
                        facts.status_entries
                    ),
                    muted,
                ),
            ]),
            Line::from(""),
        ];
        if git.diff.is_empty() {
            lines.push(Line::styled("nothing changed here", muted));
        }
        // Two rows of chrome above and two below, so the diff never paints over its own border.
        let rows = usize::from(height.saturating_sub(6));
        for line in git.diff.iter().skip(git.scroll).take(rows) {
            let style = if line.starts_with("+++") || line.starts_with("---") {
                Style::default()
                    .fg(self.theme.text)
                    .add_modifier(Modifier::BOLD)
            } else if line.starts_with("diff ") || line.starts_with("index ") {
                muted
            } else if line.starts_with("@@") {
                Style::default().fg(self.theme.accent)
            } else if line.starts_with('+') {
                Style::default().fg(self.theme.done)
            } else if line.starts_with('-') {
                Style::default().fg(self.theme.blocked)
            } else {
                Style::default().fg(self.theme.text)
            };
            lines.push(Line::styled(line.clone(), style));
        }
        let more = git.diff.len().saturating_sub(git.scroll + rows);
        lines.push(Line::from(""));
        lines.push(Line::styled(
            if more > 0 {
                format!("j/k scroll · g/G ends · {more} more lines · Esc closes")
            } else {
                "j/k scroll · g/G ends · Esc closes".to_owned()
            },
            muted,
        ));
        frame.render_widget(
            Paragraph::new(lines)
                .style(Style::default().fg(self.theme.text).bg(self.theme.surface))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(Theme::border_type())
                        .border_style(Style::default().fg(self.theme.border_focused))
                        .title(" GIT "),
                ),
            popup,
        );
    }

    /// Adds a task to Dock's own board from whatever was typed into the chooser.
    fn create_task(&mut self, title: &str) -> UiCommand {
        let Some(directory) = self.board_dir.clone() else {
            self.error = Some("no board is open".into());
            return UiCommand::None;
        };
        if !self.board_is_personal {
            self.error = Some(
                "this is the repository's board — add tasks with kanban-md so its history stays \
                 the repository's"
                    .into(),
            );
            return UiCommand::None;
        }
        if title.trim().is_empty() {
            self.error = Some("type a title first, then Enter adds it".into());
            return UiCommand::None;
        }
        match crate::board::create(&directory, title) {
            Ok(task) => {
                self.error = Some(format!("added task {}: {}", task.id, task.title));
                // Re-read rather than pushing the new task onto the open list: the board is files
                // on disk, and anything else may have written to it since it was opened.
                UiCommand::LoadBoard
            }
            Err(message) => {
                self.error = Some(message);
                UiCommand::None
            }
        }
    }

    /// Receives the board and opens it as columns of cards.
    pub fn set_board_tasks(&mut self, tasks: Vec<BoardTask>, directory: std::path::PathBuf) {
        self.set_board_pane_tasks(tasks.clone(), directory.clone());
        let writable = self.board_is_personal;
        self.board = Some(BoardOverlay {
            view: BoardView::new(tasks),
            directory,
            writable,
            composing: None,
        });
        self.error = None;
    }

    /// Receives the board without opening anything over the canvas.
    ///
    /// A Board pane is already on screen, so reading its files must not also pop a modal in front
    /// of it — which is what made this a second entry point rather than a flag. It is also the
    /// path a board pane takes on start-up, because a pane is not opened by a keystroke and
    /// nothing else in the loop would ever read the board for it.
    pub fn set_board_pane_tasks(&mut self, tasks: Vec<BoardTask>, directory: std::path::PathBuf) {
        self.board_is_personal = crate::board::is_personal(&directory);
        self.board_pane_view = Some(BoardView::new(tasks.clone()));
        self.board_tasks = tasks;
        self.board_dir = Some(directory);
    }

    /// Whether a Board pane is on the canvas with no board read for it yet.
    ///
    /// The board is files on disk that only the client can see, and the overlay is the only thing
    /// that ever asked for them — so a board pane restored from a previous session would have come
    /// back as an empty grid until somebody pressed `Ctrl+B k`. Answered from a field first, so
    /// the common case costs one `Option` check rather than a walk over every pane on every frame.
    pub fn board_pane_needs_load(&self) -> bool {
        self.board_dir.is_none()
            && self
                .layout
                .workspaces
                .iter()
                .any(|workspace| workspace.panes.values().any(|pane| pane.is_board()))
    }

    fn board_key(&mut self, key: KeyEvent) -> UiCommand {
        let Some(board) = self.board.as_mut() else {
            return UiCommand::None;
        };
        // A title being typed owns every printable key, so the single-letter controls below are
        // live only when one is not. Esc unwinds a level at a time, as copy mode does: abandoning
        // a half-typed title should not also close the board behind it.
        if let Some(title) = board.composing.as_mut() {
            match key.code {
                KeyCode::Esc => board.composing = None,
                KeyCode::Backspace => {
                    title.pop();
                }
                KeyCode::Enter => {
                    let title = title.clone();
                    board.composing = None;
                    return self.create_task(&title);
                }
                KeyCode::Char(character)
                    if !key.modifiers.intersects(
                        KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                    ) =>
                {
                    title.push(character)
                }
                _ => {}
            }
            return UiCommand::None;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.board = None,
            // Through the board's own cursor rather than through the view's, because `ACTIVE`
            // holds entries no view can name: the cursor walks what the grid draws, in the order
            // the grid draws it, or `j` would jump about wherever that column has re-sorted.
            KeyCode::Left | KeyCode::Char('h') => return self.move_board_cursor(-1, 0),
            KeyCode::Right | KeyCode::Char('l') => return self.move_board_cursor(1, 0),
            KeyCode::Up | KeyCode::Char('k') => return self.move_board_cursor(0, -1),
            KeyCode::Down | KeyCode::Char('j') => return self.move_board_cursor(0, 1),
            KeyCode::Char('n') if board.writable => board.composing = Some(String::new()),
            KeyCode::Char('n') => {
                self.error =
                    Some("this board is the repository's — add tasks with kanban-md".into())
            }
            // `<` and `>` move the card itself, which is the one thing a board is for that a list
            // cannot do at all.
            KeyCode::Char('<' | ',') => return self.shift_task(-1),
            KeyCode::Char('>' | '.') => return self.shift_task(1),
            KeyCode::Enter => return self.dispatch_selected_task(),
            _ => {}
        }
        UiCommand::None
    }

    /// Moves the selected card one column, and follows it there.
    fn shift_task(&mut self, delta: isize) -> UiCommand {
        let Some(board) = self.board.as_mut() else {
            return UiCommand::None;
        };
        if !board.writable {
            self.error = Some("this board is the repository's — move tasks with kanban-md".into());
            return UiCommand::None;
        }
        // Through the board's one cursor rather than the view's own row, because the cursor can
        // be on a live agent that has no card: `>` on one of those has nothing to move, and
        // moving whatever card the view's row happened to be over would move the wrong thing.
        let Some(id) = self.cursor_card() else {
            self.error = Some("that is a running agent, not a card".into());
            return UiCommand::None;
        };
        let Some(board) = self.board.as_ref() else {
            return UiCommand::None;
        };
        let Some(status) = board
            .view
            .tasks()
            .iter()
            .find(|task| task.id == id)
            .map(|task| task.status.clone())
        else {
            return UiCommand::None;
        };
        // The board's own columns, not the constant's. A card sitting in a status Dock has never
        // heard of is exactly the card a person most wants to move, and resolving its position
        // through `STATUSES` made that the one card `<` and `>` refused to touch.
        let columns: Vec<String> = board.view.statuses().to_vec();
        let Some(current) = columns.iter().position(|known| *known == status) else {
            self.error = Some(format!("task {id} is in an unknown column: {status}"));
            return UiCommand::None;
        };
        let next = current
            .saturating_add_signed(delta)
            .min(columns.len().saturating_sub(1));
        if next == current {
            return UiCommand::None;
        }
        let directory = board.directory.clone();
        match crate::board::set_status(&directory, id, &columns[next]) {
            Ok(_) => {
                // Re-read rather than editing the copy in hand: the board is files on disk and
                // anything else may have written to it since it was opened.
                let tasks = crate::board::load(&directory);
                // Both views, because a Board pane may be on the canvas underneath this overlay
                // and a card that moved in one and not the other is two answers to one question.
                self.set_board_pane_tasks(tasks.clone(), directory.clone());
                if let Some(view) = self.board_pane_view.as_mut() {
                    view.follow(id);
                }
                if let Some(board) = self.board.as_mut() {
                    board.view = BoardView::new(tasks);
                    board.view.follow(id);
                }
                // The one cursor goes with the card, which is what `follow` does for each view:
                // a card moved out from under the cursor would otherwise leave it on whatever
                // slid into that position.
                self.set_board_cursor(next, Some(BoardTarget::Card(id)));
                self.error = None;
            }
            Err(message) => self.error = Some(message),
        }
        UiCommand::None
    }

    /// Puts an agent on the selected card.
    fn dispatch_selected_task(&mut self) -> UiCommand {
        if self.board.is_none() {
            return UiCommand::None;
        }
        // The cursor, not the view's row: `ACTIVE` holds live agents that were never dispatched
        // from a card, and there is nothing to put an agent on there because one is already on it.
        let Some(id) = self.cursor_card() else {
            self.error = Some("that is a running agent, not a card".into());
            return UiCommand::None;
        };
        let key = id.to_string();
        self.board = None;
        self.task_dispatch_for(&key)
    }

    /// Assembles the dispatch for one card: the workspace and pane it lands in, the task it
    /// carries, and the agent that will run it.
    ///
    /// Dispatching is the one thing the board does that changes something outside Dock, so this
    /// carries the whole context the dispatch needs rather than leaving the caller to rebuild it
    /// from a stale view of the layout. It reads the card out of `board_tasks` by id rather than
    /// taking the one in hand, because the board is files on disk and the card may have been
    /// removed since the overlay was opened.
    ///
    /// This was an arm of `take_picked` until the list-style board picker went away. Nothing
    /// opened that picker any more, so the arm was reachable only through this one call and the
    /// enum variant that named it was a purpose no picker ever had.
    fn task_dispatch_for(&mut self, task_key: &str) -> UiCommand {
        let Some(workspace) = self.workspace() else {
            self.error = Some("cannot dispatch: create a workspace first".into());
            return UiCommand::None;
        };
        let workspace_id = workspace.workspace_id.clone();
        let pane_id = workspace.focused_pane_id.clone();
        let Some(task) = self
            .board_tasks
            .iter()
            .find(|task| task.id.to_string() == task_key)
        else {
            self.error = Some("that task is no longer on the board".into());
            return UiCommand::None;
        };
        let (task_id, title) = (task.id, task.title.clone());
        let Some(adapter) = self.dispatch_adapter() else {
            self.error = Some(
                "cannot dispatch: no agent is installed (claude, codex, amp or copilot)".into(),
            );
            return UiCommand::None;
        };
        self.error = None;
        let run_id = self.next_unique_id("dock_task");
        UiCommand::DispatchTask(TaskDispatch {
            workspace_id,
            pane_id,
            run_id,
            task_id,
            title,
            adapter,
        })
    }

    /// Receives the pending handoffs and opens the review overlay over them.
    pub fn set_review_inbox(&mut self, items: Vec<(HandoffRecord, Option<ReviewDecision>)>) {
        if items.is_empty() {
            self.review = None;
            self.error = Some("nothing has been handed back yet".into());
            return;
        }
        // Undecided first: those are the ones asking something of the reader. Everything else is
        // history, and history that pushed the open questions down the list would be worse than
        // no history at all.
        let mut items = items;
        items.sort_by_key(|(_, decision)| decision.is_some());
        self.review = Some(ReviewOverlay {
            items,
            selected: 0,
            pending: None,
        });
        self.error = None;
    }

    fn review_key(&mut self, key: KeyEvent) -> UiCommand {
        let Some(review) = self.review.as_mut() else {
            return UiCommand::None;
        };
        // While a note is being typed every printable key belongs to it, so the route keys are
        // live only before one is started. Esc unwinds a level at a time, as copy mode does:
        // abandoning a half-typed note should not also close the queue behind it.
        if let Some((route, note)) = review.pending.as_mut() {
            match key.code {
                KeyCode::Esc => review.pending = None,
                KeyCode::Backspace => {
                    note.pop();
                }
                KeyCode::Enter => {
                    if note.trim().is_empty() {
                        self.error =
                            Some("a decision needs a note saying why, however short".into());
                        return UiCommand::None;
                    }
                    let route = *route;
                    let note = note.clone();
                    let run_id = review.items[review.selected].0.packet.run_id.clone();
                    // The overlay closes on send. Whether the decision stuck is the daemon's to
                    // say, and it is re-read from the inbox rather than assumed here.
                    self.review = None;
                    return UiCommand::Request(Box::new(Request::Decide(DecideRequest {
                        run_id,
                        route,
                        note,
                    })));
                }
                KeyCode::Char(character)
                    if !key.modifiers.intersects(
                        KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                    ) =>
                {
                    note.push(character)
                }
                _ => {}
            }
            return UiCommand::None;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.review = None,
            KeyCode::Up => review.selected = review.selected.saturating_sub(1),
            KeyCode::Down => {
                review.selected = (review.selected + 1).min(review.items.len().saturating_sub(1))
            }
            KeyCode::Char('a') => review.pending = Some((ReviewRoute::AcceptScope, String::new())),
            KeyCode::Char('c') => {
                review.pending = Some((ReviewRoute::RequestChange, String::new()))
            }
            _ => {}
        }
        UiCommand::None
    }

    /// The review overlay: what an agent handed back, and the two things a human can say about it.
    fn render_review(&self, frame: &mut Frame, area: Rect) {
        let Some(review) = self.review.as_ref() else {
            return;
        };
        let width = area.width.min(72);
        let height = area.height.min(20);
        let popup = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, popup);
        let heading = Style::default()
            .fg(self.theme.accent)
            .add_modifier(Modifier::BOLD);
        let muted = Style::default().fg(self.theme.muted);

        let mut lines = Vec::new();
        for (index, (record, decision)) in review.items.iter().enumerate() {
            let selected = index == review.selected;
            // An answered handoff wears its answer on the row, so the list reads as a history
            // rather than as a queue that has mysteriously stopped asking for anything.
            let verdict = match decision {
                Some(decision) => match decision.route {
                    ReviewRoute::AcceptScope => "  accepted",
                    ReviewRoute::RequestChange => "  changes requested",
                },
                None => "  awaiting you",
            };
            lines.push(Line::from(vec![
                Span::styled(
                    if selected { "> " } else { "  " },
                    Style::default().fg(self.theme.accent),
                ),
                Span::styled(
                    format!("{}  ", record.packet.task_id),
                    if selected { heading } else { muted },
                ),
                Span::styled(record.packet.run_id.clone(), muted),
                Span::styled(
                    verdict,
                    Style::default().fg(if decision.is_some() {
                        self.theme.done
                    } else {
                        self.theme.blocked
                    }),
                ),
            ]));
            if !selected {
                continue;
            }
            lines.push(Line::styled(
                format!("    {}", record.packet.summary),
                Style::default().fg(self.theme.text),
            ));
            if let Some(question) = &record.packet.question {
                lines.push(Line::styled(format!("    ? {question}"), heading));
            }
            if !record.packet.checks.is_empty() {
                let checks = record
                    .packet
                    .checks
                    .iter()
                    .map(|check| {
                        format!(
                            "{} {}",
                            check.name,
                            if check.passed { "ok" } else { "failed" }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("   ");
                lines.push(Line::styled(format!("    {checks}"), muted));
            }
            // Evidence the daemon measured, not anything the agent asserted about itself.
            lines.push(Line::styled(
                format!(
                    "    {} files  +{} -{}  on {}",
                    record.evidence.changed_files,
                    record.evidence.insertions,
                    record.evidence.deletions,
                    record.evidence.branch
                ),
                muted,
            ));
            lines.push(Line::from(""));
        }
        lines.push(Line::from(""));
        lines.push(match review.pending.as_ref() {
            Some((route, note)) => Line::from(vec![
                Span::styled(
                    match route {
                        ReviewRoute::AcceptScope => "accept · why: ",
                        ReviewRoute::RequestChange => "request changes · why: ",
                    },
                    heading,
                ),
                Span::styled(
                    format!("{note}\u{2588}"),
                    Style::default().fg(self.theme.text),
                ),
            ]),
            None => Line::styled(
                "a accept scope · c request changes · up/down select · Esc close",
                muted,
            ),
        });
        if review.pending.is_some() {
            lines.push(Line::styled(
                "Enter records the decision · Esc keeps the queue open",
                muted,
            ));
        } else {
            // The invariant this whole queue exists to protect, said where it is acted on.
            lines.push(Line::styled(
                "A decision is recorded, never merged: Dock does not touch Git or close the task.",
                muted,
            ));
        }
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(self.theme.text).bg(self.theme.surface))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(Theme::border_type())
                        .border_style(Style::default().fg(self.theme.border_focused))
                        .title(" REVIEW "),
                ),
            popup,
        );
    }

    /// Relaunches the agent that last ran in this pane, telling it to continue its most recent
    /// session rather than start a new one.
    ///
    /// The agent is a Dock-launched process either way, which is the point: Dock's owned-process
    /// invariant survives intact, because it still only ever signals groups it created and never
    /// adopts one it did not. What persists across the relaunch is the agent's own transcript,
    /// which it stores itself and finds again from the working directory — so this outlives the
    /// pane's process dying, the daemon restarting, and the machine rebooting, none of which
    /// adopting a live process could have survived.
    fn resume_agent(&mut self) -> UiCommand {
        let Some(workspace) = self.workspace() else {
            self.error = Some("resume unavailable: create a workspace first".into());
            return UiCommand::None;
        };
        let workspace_id = workspace.workspace_id.clone();
        let pane_id = workspace.focused_pane_id.clone();
        // The pane keeps its run binding after the run exits, which is exactly the case this
        // command exists for: the agent is gone and its conversation is what remains.
        let Some(run) = self.focused_run().cloned() else {
            self.error = Some("resume unavailable: no agent has run in this pane".into());
            return UiCommand::None;
        };
        let Some(arguments) = run.adapter.resume_arguments() else {
            self.error = Some(format!("{} cannot be resumed", run.adapter.label()));
            return UiCommand::None;
        };
        let arguments: Vec<String> = arguments.iter().map(|a| (*a).to_string()).collect();
        let run_id = self.next_unique_id("dock_ui");
        self.error = None;
        match run.binding_kind {
            // A repository-bound run carries the task and worktree its conversation belongs to,
            // so the resumed run is bound to exactly the same ones.
            BindingKind::Repository => {
                UiCommand::Request(Box::new(Request::LaunchIntoPane(LaunchIntoPaneRequest {
                    workspace_id,
                    pane_id,
                    dispatch: DispatchRequest {
                        repository_root: run.repository_root.clone(),
                        external_task_ref: run.external_task_ref.clone(),
                        run_id,
                        worktree: run.worktree.clone(),
                        adapter: AdapterSelection {
                            id: run.adapter.clone(),
                            executable: None,
                            arguments,
                        },
                    },
                })))
            }
            // An unbound run has no task, only a directory — and the directory is the whole of
            // what the agent needs, since that is where it filed the conversation.
            BindingKind::Terminal => {
                let Ok(profile) = DashboardProfile::try_from(run.adapter.clone()) else {
                    self.error = Some(format!("{} cannot be resumed", run.adapter.label()));
                    return UiCommand::None;
                };
                UiCommand::Request(Box::new(Request::TerminalLaunch(TerminalLaunchRequest {
                    workspace_id,
                    pane_id,
                    run_id,
                    profile,
                    runtime_directory: run.worktree.clone(),
                    arguments,
                    // A resume re-enters the run's own pane, and the daemon already knows what
                    // that run was dispatched for.
                    external_task_ref: String::new(),
                })))
            }
        }
    }

    /// Opens the file chooser, rooted where the focused pane actually is.
    ///
    /// The pane's own working directory comes first, which shells report through OSC 7 as they
    /// move, so the listing follows a `cd` rather than staying pinned to wherever the pane started.
    /// A pane with no run, or one whose shell never reported, falls back to the repository this
    /// dashboard was bound to.
    fn open_file_picker(&mut self) -> UiCommand {
        let root = self
            .focused_run_id()
            .and_then(|run_id| self.runs.iter().find(|run| run.run_id == run_id))
            .and_then(|run| run.cwd.clone())
            .filter(|cwd| !cwd.is_empty())
            .unwrap_or_else(|| self.repository_root.clone());
        if root.is_empty() {
            self.error = Some("file picker unavailable: this pane has no directory".into());
            return UiCommand::None;
        }
        let listed = files::list(Path::new(&root), files::LISTING_LIMIT);
        if listed.is_empty() {
            self.error = Some(format!("no files under {root}"));
            return UiCommand::None;
        }
        // Only the directory is shown as detail: the file name is what gets matched and what the
        // eye lands on, and a full path repeated on every row is noise the width cannot afford.
        let items = listed
            .into_iter()
            .map(|path| {
                let (directory, name) = match path.rsplit_once('/') {
                    Some((directory, name)) => (directory.to_owned(), name.to_owned()),
                    None => (String::new(), path.clone()),
                };
                PickerItem {
                    key: path,
                    label: name,
                    detail: directory,
                }
            })
            .collect();
        self.picker = Some((PickerPurpose::File, Picker::new(items)));
        self.error = None;
        UiCommand::None
    }

    /// Jumps to a workspace by its 1-based position, the one shown on its tab.
    fn jump_to_workspace(&mut self, position: u8) -> UiCommand {
        let index = usize::from(position).saturating_sub(1);
        if index >= self.layout.workspaces.len() {
            self.error = Some(format!("no workspace {position}"));
            return UiCommand::None;
        }
        self.workspace_index = index;
        self.disarm_workspace_close();
        self.error = None;
        UiCommand::None
    }

    fn picker_key(&mut self, key: KeyEvent) -> UiCommand {
        let Some((purpose, picker)) = self.picker.as_mut() else {
            return UiCommand::None;
        };
        match key.code {
            KeyCode::Esc => self.picker = None,
            KeyCode::Up => picker.move_selection(-1),
            KeyCode::Down => picker.move_selection(1),
            KeyCode::Backspace => picker.pop(),
            KeyCode::Enter => {
                let taken = picker.selected().map(|item| (*purpose, item.key.clone()));
                match taken {
                    Some((purpose, key)) => {
                        self.picker = None;
                        return self.take_picked(purpose, &key);
                    }
                    None => self.picker = None,
                }
            }
            // Every printable character is query text. Only the chording modifiers are excluded —
            // SHIFT must not be, because crossterm reports it on every capital letter and a
            // workspace named `API` has to be reachable by typing it.
            KeyCode::Char(character)
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
            {
                picker.push(character)
            }
            _ => {}
        }
        UiCommand::None
    }

    fn take_picked(&mut self, purpose: PickerPurpose, key: &str) -> UiCommand {
        match purpose {
            PickerPurpose::Workspace => {
                // A workspace can close between opening the picker and taking a row, so this
                // looks the id up again rather than trusting the position it had when listed.
                match self
                    .layout
                    .workspaces
                    .iter()
                    .position(|workspace| workspace.workspace_id == key)
                {
                    Some(index) => {
                        self.workspace_index = index;
                        self.disarm_workspace_close();
                        self.error = None;
                    }
                    None => self.error = Some("that workspace is gone".into()),
                }
                UiCommand::None
            }
            // The path is typed into the pane rather than opened, because Dock cannot know which
            // verb was wanted. Reaching for it after `vim ` opens it; reaching for it mid-sentence
            // to an agent hands the agent a path. A trailing space is deliberate: a path is almost
            // never the last thing typed on a line.
            PickerPurpose::File => {
                let mut typed = key.to_owned();
                typed.push(' ');
                self.error = None;
                self.send_to_pane(typed.into_bytes())
            }
        }
    }

    /// Moves the visible workspace, saturating at both ends rather than wrapping: `.` on the
    /// last workspace is a mis-press, and jumping silently back to the first would move the
    /// user somewhere they did not ask to go.
    ///
    /// Purely local, like zoom — the daemon has no notion of which workspace this client is
    /// looking at, so there is nothing to tell it.
    fn select_workspace(&mut self, delta: i8) -> UiCommand {
        let Some(last) = self.layout.workspaces.len().checked_sub(1) else {
            self.error = Some("workspace unavailable: create a workspace first".into());
            return UiCommand::None;
        };
        self.workspace_index = if delta < 0 {
            self.workspace_index
                .saturating_sub(usize::from(delta.unsigned_abs()))
        } else {
            self.workspace_index
                .saturating_add(usize::from(delta.unsigned_abs()))
                .min(last)
        };
        self.disarm_workspace_close();
        self.error = None;
        UiCommand::None
    }

    /// Toggles a full-area view of the focused pane. Zoom is local to this client: the
    /// daemon's layout tree is unchanged, so it costs no request, but the pane's inner
    /// geometry does change and the next frame announces the new PTY size.
    fn zoom(&mut self) -> UiCommand {
        let Some(workspace) = self.workspace() else {
            self.error = Some("zoom unavailable: create a workspace first".into());
            return UiCommand::None;
        };
        let focused = workspace.focused_pane_id.clone();
        self.zoomed = match self.zoomed.take() {
            Some(current) if current == focused => None,
            _ => Some(focused),
        };
        self.error = None;
        UiCommand::None
    }

    /// Whether the focused pane's viewport is frozen for selection.
    pub fn copy_mode(&self) -> bool {
        self.copy.is_some()
    }

    /// The mode indicator. Copy mode swallows every key, so it has to announce itself: an
    /// input mode with no on-screen trace is indistinguishable from a hung dashboard.
    pub fn copy_status(&self) -> Option<String> {
        let session = &self.copy.as_ref()?.session;
        if self.copy_searching {
            return Some(format!(
                "COPY /{}",
                session.search_query().unwrap_or_default()
            ));
        }
        let (row, column) = session.cursor();
        let verb = if session.selecting() {
            "SELECTING"
        } else {
            "MOVE"
        };
        // CONTROLLER ADDENDUM: grid coordinates stay 0-based everywhere inside; only this
        // presentation boundary counts from one, because that is how every editor and pager
        // the user knows reports a line and column.
        Some(format!(
            "COPY {verb} {},{}",
            row.saturating_add(1),
            column.saturating_add(1)
        ))
    }

    /// Freezes the focused pane for keyboard selection, starting at its live cursor.
    ///
    /// The freeze is a clone of the pane's screen taken here and held for the life of the
    /// session. The live parser is deliberately left running: see [`CopyMode`] for why
    /// stopping it — or buffering what it would have consumed — is the worse of the two.
    fn enter_copy_mode(&mut self) -> UiCommand {
        let Some(run_id) = self.focused_run_id().map(str::to_owned) else {
            self.error =
                Some("copy mode unavailable: this pane has no run · Ctrl+B l launches one".into());
            return UiCommand::None;
        };
        // A pane whose screen has not arrived yet has nothing to freeze, and a mode that
        // swallows every key over an empty grid is a dashboard that looks hung.
        let Some(screen) = self.screens.get(&run_id) else {
            self.error = Some("copy mode unavailable: this pane has not painted yet".into());
            return UiCommand::None;
        };
        self.copy = Some(CopyMode::new(run_id, screen.cursor(), screen.snapshot()));
        self.copy_searching = false;
        self.error = None;
        UiCommand::None
    }

    /// Every key while copy mode is active, so none of them can reach the PTY.
    ///
    /// The mode is taken out of `self` for the duration: the handlers need `self.error` and
    /// the frozen screen at the same time, and leaving it in place would borrow `self` twice.
    fn copy_key(&mut self, key: KeyEvent) -> UiCommand {
        let Some(mut mode) = self.copy.take() else {
            return UiCommand::None;
        };
        // The frozen grid, not the live one: every coordinate in the session is a cell of the
        // screen the mode was opened on, and a live resize must not silently re-scale them.
        let bounds = mode.frozen.size();
        // Esc unwinds one level at a time: the prompt first, then the mode. The invariant is
        // that a small bounded number of presses always reaches the live pane, not that one
        // press escapes every level — which is exactly what a rename form already does.
        if key.code == KeyCode::Esc && !self.copy_searching {
            self.leave_copy_mode(&mode.session.run_id);
            return UiCommand::None;
        }
        if self.copy_searching {
            self.copy_search_key(key, &mut mode, bounds);
            self.copy = Some(mode);
            return UiCommand::None;
        }
        match key.code {
            // A composed letter is somebody reaching past copy mode, not a motion: without
            // this `Ctrl+H` moves left and `Ctrl+Y` yanks. Shift stays allowed because
            // crossterm reports uppercase `G` and `N` with it set.
            KeyCode::Char(_) if composed(key) => {}
            KeyCode::Char('q') => {
                self.leave_copy_mode(&mode.session.run_id);
                return UiCommand::None;
            }
            KeyCode::Char('h') | KeyCode::Left => mode.step(0, -1, bounds),
            KeyCode::Char('j') | KeyCode::Down => mode.step(1, 0, bounds),
            KeyCode::Char('k') | KeyCode::Up => mode.step(-1, 0, bounds),
            KeyCode::Char('l') | KeyCode::Right => mode.step(0, 1, bounds),
            // Deliberately the top and bottom of the *viewport*, not of history: `g` in a pane
            // scrolled back must reach the row above the cursor, not jump the user thousands of
            // rows away from what they are reading. `k` past the top edge walks into history a
            // row at a time (see `CopyMode::step`).
            KeyCode::Char('g') => mode.session.set_cursor((0, 0), bounds),
            KeyCode::Char('G') => mode
                .session
                .set_cursor((bounds.0.saturating_sub(1), 0), bounds),
            KeyCode::Char('v') => mode.session.begin_selection(),
            KeyCode::Char('y') => {
                self.yank(&mode);
                self.leave_copy_mode(&mode.session.run_id);
                return UiCommand::None;
            }
            KeyCode::Char('/') => {
                mode.session.begin_search();
                self.copy_searching = true;
                self.error = None;
            }
            KeyCode::Char('n') => self.copy_jump(&mut mode, true, bounds),
            KeyCode::Char('N') => self.copy_jump(&mut mode, false, bounds),
            _ => {}
        }
        self.copy = Some(mode);
        UiCommand::None
    }

    /// Keys typed at the `/` prompt. Enter closes the prompt and jumps; the query survives so
    /// `n`/`N` can keep walking the same matches.
    fn copy_search_key(&mut self, key: KeyEvent, mode: &mut CopyMode, bounds: (u16, u16)) {
        match key.code {
            // Same rule as the mode's own bindings: `Ctrl+C` must not type a `c` into the query.
            KeyCode::Char(_) if composed(key) => {}
            KeyCode::Char(character) => mode.session.push_search(character),
            KeyCode::Esc => {
                mode.session.cancel_search();
                self.copy_searching = false;
            }
            KeyCode::Backspace => {
                if mode.session.search_query().is_some_and(str::is_empty) {
                    mode.session.cancel_search();
                    self.copy_searching = false;
                } else {
                    mode.session.pop_search();
                }
            }
            KeyCode::Enter => {
                self.copy_searching = false;
                self.copy_jump(mode, true, bounds);
            }
            _ => {}
        }
    }

    /// Jumps to the next or previous hit for the standing query, or says why it could not.
    ///
    /// Searches the frozen rows, which are the rows the user is looking at. Reading the live
    /// screen would land the cursor on a coordinate whose text is not the text that matched.
    fn copy_jump(&mut self, mode: &mut CopyMode, forward: bool, bounds: (u16, u16)) {
        let Some(query) = mode.session.search_query().map(str::to_owned) else {
            self.error = Some("no search yet · / starts one".into());
            return;
        };
        let rows: Vec<String> = (0..bounds.0)
            .map(|row| mode.frozen.visible_row(row))
            .collect();
        if mode
            .session
            .jump_to_match(&find_matches(&rows, &query), forward, bounds)
        {
            self.error = None;
        } else {
            self.error = Some(format!("no matches for {query:?}"));
        }
    }

    /// Puts the selection on the clipboard and names the route it took.
    ///
    /// A bare `y` with no anchor yanks the cursor's line rather than refusing: the dominant
    /// reason to press `y` in a terminal is "give me that line" — a URL, a path, a stack
    /// frame — and demanding `v$y` for it is friction on the most common copy there is. The
    /// line yank names itself in the notice so a user who meant something else sees it at once.
    ///
    /// The route is reported because OSC 52 is disabled by default in some terminals: a yank
    /// that silently reached nothing looks exactly like a yank that worked.
    fn yank(&mut self, mode: &CopyMode) {
        // The frozen screen, so the clipboard gets the characters the highlight was over
        // rather than whatever output has since scrolled through those cells.
        let screen = &mode.frozen;
        let (text, subject) = match mode.session.selection() {
            Some((from, to)) => {
                let text = screen.selection_text(from, to);
                let count = text.chars().count();
                (text, format!("{count} characters"))
            }
            None => {
                let row = mode.session.cursor().0;
                // Trailing blanks are grid padding, not content: nobody wants 60 spaces
                // pasted after the path they just copied.
                let text = screen.visible_row(row).trim_end().to_owned();
                let count = text.chars().count();
                // 1-based for the same reason `copy_status` is, and it has to agree with it:
                // seeing the same line called 0 in one place and 1 in another reads as a bug.
                (
                    text,
                    format!("line {} ({count} characters)", row.saturating_add(1)),
                )
            }
        };
        self.record_copy(text, &subject);
    }

    /// Sends one piece of text to the clipboard and reports, exactly, which routes ran.
    ///
    /// The notice deliberately no longer says "to the clipboard": OSC 52 is one-way and three
    /// of the most common hosts (Terminal.app always, iTerm2 and tmux by default) drop it in
    /// silence, so Dock naming the clipboard as a completed destination was a claim it had no
    /// way to check. `ClipboardRoute::describe` says what was actually asked of whom.
    fn record_copy(&mut self, text: String, subject: &str) {
        self.error = Some(match clipboard::copy(&text) {
            Ok(routes) => {
                let routes = routes
                    .into_iter()
                    .map(ClipboardRoute::describe)
                    .collect::<Vec<_>>()
                    .join(" and ");
                // Remembered only when a route ran, so a middle click can never paste back
                // text that no clipboard anywhere ever received.
                self.last_copied = Some(text);
                format!("copied {subject} \u{b7} {routes}")
            }
            Err(reason) => format!("copy failed: {reason}"),
        });
    }

    /// Copies whatever the pointer gesture just selected.
    ///
    /// Blank selections are dropped rather than copied: a press that jitters one cell, or a
    /// drag across a pane's padding, would otherwise replace a clipboard the user had filled
    /// deliberately with a run of spaces. Auto-copy is only worth having if it cannot destroy
    /// something.
    fn copy_pointer_selection(&mut self) {
        let Some(text) = self
            .copy
            .as_ref()
            .and_then(|mode| {
                let (from, to) = mode.session.selection()?;
                Some(mode.frozen.selection_text(from, to))
            })
            .filter(|text| !text.trim().is_empty())
        else {
            return;
        };
        let subject = format!("{} characters", text.chars().count());
        self.record_copy(text, &subject);
    }

    /// Pastes the last copied text into the pane under the pointer.
    ///
    /// Middle click is the X11 convention and right click is Windows', and terminals honour
    /// whichever the platform uses; Dock takes both rather than guessing. The press must land
    /// in the *focused* pane's body — a paste is destructive input, and a click that lands in
    /// one pane must never be typed into another.
    fn paste_last_copied(&mut self, column: u16, row: u16) -> UiCommand {
        let over_focused = self
            .workspace()
            .map(|workspace| workspace.focused_pane_id.clone())
            .and_then(|pane_id| self.pane_inner_areas.get(&pane_id).copied())
            .is_some_and(|inner| contains(inner, column, row));
        if !over_focused {
            return UiCommand::None;
        }
        let Some(text) = self.last_copied.clone() else {
            self.error = Some("nothing copied yet \u{b7} select some text first".into());
            return UiCommand::None;
        };
        self.error = None;
        // Through the same encoder the host's own bracketed paste uses, so a multi-line paste
        // cannot execute line by line in a shell that asked for bracketing.
        self.paste(text)
    }

    /// How many presses in a row this one is, for double- and triple-click selection.
    ///
    /// A press one column either side of the last still counts: a hand that moves a single
    /// cell between two clicks meant to double-click, and requiring the exact same cell made
    /// double click fail often enough to feel broken. Rows must match exactly — a press a row
    /// away is a different line and unambiguously a new gesture.
    fn count_click(&mut self, column: u16, row: u16) -> u8 {
        let at = Instant::now();
        let count = match self.last_click {
            Some(previous)
                if at.duration_since(previous.at) <= MULTI_CLICK_WINDOW
                    && previous.row == row
                    && previous.column.abs_diff(column) <= 1 =>
            {
                previous.count.saturating_add(1)
            }
            _ => 1,
        };
        self.last_click = Some(Click {
            at,
            column,
            row,
            count,
        });
        count
    }

    /// Selects the word or the line under a double or triple click.
    ///
    /// Nothing is selected when the click lands on blank padding, so a double click in the
    /// empty half of a pane does not arm a copy of nothing.
    fn select_by_click(&mut self, clicks: u8) {
        let Some(drag) = self.pane_drag.clone() else {
            return;
        };
        // Read the row from the frozen screen when this pane is already frozen. A second
        // click inside an open selection must land on the characters the user can see, and
        // the live screen may have scrolled several times since the mode opened.
        let frozen_here = self.copy.as_ref().filter(|mode| mode.is_for(&drag.run_id));
        let Some((bounds, row_text)) = frozen_here
            .map(|mode| (mode.frozen.size(), mode.frozen.visible_row(drag.origin.0)))
            .or_else(|| {
                self.screens
                    .get(&drag.run_id)
                    .map(|screen| (screen.size(), screen.visible_row(drag.origin.0)))
            })
        else {
            return;
        };
        let selection = if clicks >= 3 {
            line_bounds(&row_text)
        } else {
            word_bounds(&row_text, drag.origin.1)
        };
        // Deliberately before anything is committed: a double click on blank padding selects
        // nothing, and must therefore not freeze a pane or take the keyboard either.
        let Some((first, last)) = selection else {
            return;
        };
        if let Some(stale) = self.stale_copy_run(&drag.run_id) {
            self.leave_copy_mode(&stale);
        }
        let mut session = CopySession::new(drag.run_id.clone(), (drag.origin.0, first));
        session.begin_selection();
        session.set_cursor((drag.origin.0, last), bounds);
        match self.copy.as_mut() {
            // Already frozen on this pane: keep the freeze rather than re-taking it, or a
            // double click inside a scrolled-back selection would snap the view to the tail.
            Some(mode) => mode.session = session,
            None => {
                let Some(frozen) = self.screens.get(&drag.run_id).map(PaneScreen::snapshot) else {
                    return;
                };
                self.copy = Some(CopyMode { session, frozen });
            }
        }
        self.copy_searching = false;
        self.error = None;
        if let Some(armed) = self.pane_drag.as_mut() {
            armed.selected = true;
        }
    }

    /// The run of an open copy session that is *not* `run_id`, if there is one. Dragging or
    /// clicking in a different pane hands copy mode over, and the pane being left has to be
    /// released rather than staying silently frozen.
    fn stale_copy_run(&self, run_id: &str) -> Option<String> {
        self.copy
            .as_ref()
            .filter(|mode| !mode.is_for(run_id))
            .map(|mode| mode.session.run_id.clone())
    }

    /// Scrolls a pane, dragging any selection anchored in it along with the viewport.
    ///
    /// Selection endpoints are cells of the *visible* grid, so scrolling used to re-point them
    /// at whatever text moved underneath: a selection made and then scrolled yanked rows the
    /// highlight had never covered, silently. They are moved by however far the viewport
    /// actually went — `scroll_by` clamps at both ends, so the request is not the answer — and
    /// an anchor pushed off the screen ends the selection rather than clamping to an edge it
    /// no longer means.
    /// While the pane is frozen the wheel moves the *frozen* viewport, because that is the
    /// screen being painted; scrolling the live one would move nothing the user can see.
    fn scroll_pane(&mut self, run_id: &str, delta: i32) {
        if let Some(mode) = self.copy.as_mut().filter(|mode| mode.is_for(run_id)) {
            let before = mode.frozen.scroll_offset();
            mode.frozen.scroll_by(delta);
            let moved = scrolled(before, mode.frozen.scroll_offset());
            if moved == 0 {
                return;
            }
            let bounds = mode.frozen.size();
            mode.session.shift_anchor(moved, bounds);
            mode.session.shift_cursor(moved, bounds);
            return;
        }
        if let Some(screen) = self.screens.get_mut(run_id) {
            screen.scroll_by(delta);
        }
    }

    /// `Ctrl+B PageUp`: half a screen back into history, the keyboard's equivalent of a few
    /// wheel notches. Also asks for more history exactly as the wheel does — without this the
    /// keyboard would silently stop paging at the seed boundary the wheel keeps going past.
    fn scroll_page_back(&mut self) -> UiCommand {
        let Some(run_id) = self.focused_run_id().map(str::to_owned) else {
            return UiCommand::None;
        };
        let Some((rows, _)) = self.screens.get(&run_id).map(PaneScreen::size) else {
            return UiCommand::None;
        };
        self.scroll_pane(&run_id, i32::from(rows) / 2);
        match self.history_request_for(&run_id) {
            Some(request) => UiCommand::Request(Box::new(request)),
            None => UiCommand::None,
        }
    }

    /// `Ctrl+B PageDown`: half a screen forward, toward live output. Never asks for more
    /// history: paging forward only ever moves toward rows the pane already holds.
    fn scroll_page_forward(&mut self) -> UiCommand {
        let Some(run_id) = self.focused_run_id().map(str::to_owned) else {
            return UiCommand::None;
        };
        let Some((rows, _)) = self.screens.get(&run_id).map(PaneScreen::size) else {
            return UiCommand::None;
        };
        self.scroll_pane(&run_id, -(i32::from(rows) / 2));
        UiCommand::None
    }

    /// `Ctrl+B End`: back to following live output, from wherever the pane was scrolled to.
    fn scroll_to_live(&mut self) -> UiCommand {
        let Some(run_id) = self.focused_run_id().map(str::to_owned) else {
            return UiCommand::None;
        };
        let Some(offset) = self.screens.get(&run_id).map(PaneScreen::scroll_offset) else {
            return UiCommand::None;
        };
        self.scroll_pane(&run_id, -(i32::try_from(offset).unwrap_or(i32::MAX)));
        UiCommand::None
    }

    /// Extends a pointer selection, entering copy mode on the first drag of the gesture.
    ///
    /// The anchor is re-applied on every event rather than only on the first: it is always
    /// the cell the button went down on, whatever the cursor was doing beforehand, and
    /// re-applying it is cheaper than tracking whether this drag has already anchored.
    fn drag_selection(&mut self, drag: &PaneDrag, column: u16, row: u16) {
        if let Some(stale) = self.stale_copy_run(&drag.run_id) {
            // Dragging in a different pane hands copy mode over; the pane being left goes
            // back to following live output rather than staying silently frozen.
            self.leave_copy_mode(&stale);
        }
        if self.copy.is_none() {
            // The first drag of a gesture is what freezes the pane, so the pointer keeps
            // pointing at the characters it was put on however much the pane is producing.
            let Some(frozen) = self.screens.get(&drag.run_id).map(PaneScreen::snapshot) else {
                return;
            };
            self.copy = Some(CopyMode::new(drag.run_id.clone(), drag.origin, frozen));
            self.copy_searching = false;
            self.error = None;
        }
        let Some(mode) = self.copy.as_mut() else {
            return;
        };
        let bounds = mode.frozen.size();
        mode.session.set_cursor(drag.origin, bounds);
        mode.session.begin_selection();
        mode.session
            .set_cursor(clamp_cell(drag.inner, column, row), bounds);
    }

    /// Leaves copy mode and returns the pane to the live tail, which is where the user was
    /// before they froze it.
    ///
    /// Dropping the frozen screen is the entire exit path. The live parser was never stopped,
    /// so there is no backlog to replay and nothing to re-seed: the pane is already current
    /// the moment the next frame renders from it again. `scroll_to_live` is here only for the
    /// live viewport the *user* may have scrolled back before entering the mode.
    fn leave_copy_mode(&mut self, run_id: &str) {
        if let Some(screen) = self.screens.get_mut(run_id) {
            screen.scroll_to_live();
        }
        self.copy = None;
        self.copy_searching = false;
    }

    /// Ends a frozen selection whose pane has been replaced, and says why it went.
    ///
    /// A re-attach is a re-seed: the daemon sends one when a run is first seen and again
    /// whenever the pane's *geometry* changes, and a lost revision drops the replica outright.
    /// Either way the grid the selection was made against is gone. Painting the old snapshot
    /// into a differently sized rect would show the user rows that are no longer anywhere, and
    /// keeping coordinates chosen on an 80-column grid while the pane is now 120 would yank
    /// text nobody pointed at. Ending the mode loses the selection, which is a real cost — but
    /// it is visible, and it happens at the moment the user resized the thing they were
    /// selecting from, so it is not mysterious.
    fn end_copy_mode_for(&mut self, run_id: &str, reason: &str) {
        if !self.copy.as_ref().is_some_and(|mode| mode.is_for(run_id)) {
            return;
        }
        self.leave_copy_mode(run_id);
        self.error = Some(format!("copy mode ended: {reason}"));
    }

    /// The focused pane's own key encoding. A program that sets DECCKM changes what bytes
    /// an arrow key must produce, so this is read per pane rather than assumed globally.
    fn encoding_for_focused_pane(&self) -> KeyEncoding {
        self.focused_run_id()
            .and_then(|run_id| self.screens.get(run_id))
            .map(|screen| KeyEncoding {
                application_cursor: screen.screen().application_cursor(),
            })
            .unwrap_or_default()
    }

    /// The agent a dispatch will put on a task.
    ///
    /// Never the fixture. It is a test stub that prints one line and exits, and it sat at the
    /// front of the profile list, so a dashboard whose launch form had never been opened
    /// dispatched every task to it — the tasks moved to in-progress, nothing worked on them, and
    /// the pane said "exited". A product default has no business pointing at a test double.
    ///
    /// The last agent launched from the form wins, so an explicit choice is remembered; otherwise
    /// the first one actually installed. `None` means none of them are.
    pub fn dispatch_adapter(&self) -> Option<AdapterId> {
        // Which agents exist is a property of the machine, and a test that reads it is a test that
        // passes on a developer's laptop and fails on a build runner where none are installed —
        // which is exactly what it did. The override pins the answer so these tests exercise the
        // choosing, not the installing.
        #[cfg(test)]
        if let Some(pinned) = self.installed_adapters.as_ref() {
            return pinned
                .iter()
                .find(|adapter| !adapter.prompt_arguments("probe").is_empty())
                .or_else(|| pinned.first())
                .cloned();
        }
        let chosen = PROFILES
            .get(self.last_launch_profile)
            .map(|(profile, _)| AdapterId::from(*profile))
            .filter(|adapter| *adapter != AdapterId::Fixture)
            .filter(crate::adapter::builtin_available);
        chosen.or_else(|| {
            let installed = || {
                PROFILES
                    .iter()
                    .map(|(profile, _)| AdapterId::from(*profile))
                    .filter(|adapter| *adapter != AdapterId::Fixture)
                    .filter(crate::adapter::builtin_available)
            };
            // An agent that can be handed the task beats one that cannot. Dispatch exists to put
            // an agent on a specific piece of work, and an agent with no prompt positional — amp
            // takes `[options] [command]` — opens in the right place knowing nothing about why.
            // Falling back to profile order alone would pick that one first, purely alphabetically.
            installed()
                .find(|adapter| !adapter.prompt_arguments("probe").is_empty())
                .or_else(|| installed().next())
        })
    }

    /// The visible workspace's id, which is what its board is keyed by.
    pub fn workspace_id(&self) -> Option<&str> {
        self.workspace()
            .map(|workspace| workspace.workspace_id.as_str())
    }

    /// The run bound to the focused pane, if it has one.
    pub fn focused_run(&self) -> Option<&RuntimeSnapshot> {
        let run_id = self.focused_run_id()?;
        self.runs.iter().find(|run| run.run_id == run_id)
    }

    fn focused_run_id(&self) -> Option<&str> {
        self.focused_pane()?.run_id.as_deref()
    }

    fn focused_pane(&self) -> Option<&PaneLayout> {
        let workspace = self.workspace()?;
        workspace.panes.get(&workspace.focused_pane_id)
    }

    /// Routes bytes to the focused pane, or explains why they went nowhere.
    ///
    /// Input aimed at a pane with no live process used to be discarded in silence, which is
    /// indistinguishable from a frozen dashboard: the pane keeps painting the dead shell's last
    /// frame and typing simply stops having any effect. Every rejection now names the key that
    /// gets the pane working again.
    fn send_to_pane(&mut self, bytes: Vec<u8>) -> UiCommand {
        match self.focused_pane() {
            Some(pane) if pane.runtime == PaneRuntime::Exited => {
                self.error = Some("pane exited · Ctrl+B R restarts a shell here".into());
                UiCommand::None
            }
            // A pane that never had a run is not a pane that stopped working, and the daemon
            // would answer every character with an error that flickers through the footer. The
            // pane body already carries the two keys that give it a process.
            Some(pane) if pane.run_id.is_none() => UiCommand::None,
            Some(_) => UiCommand::PaneInput(bytes),
            None => UiCommand::None,
        }
    }

    /// A pasted payload, wrapped for the focused pane's own bracketed-paste mode.
    ///
    /// Without this the host terminal delivers a paste as individual key events and each line
    /// executes as it lands, which is precisely the paste-injection hazard `encode_paste` was
    /// written to close — reached by a route that never called it.
    pub fn paste(&mut self, text: String) -> UiCommand {
        if text.is_empty() {
            return UiCommand::None;
        }
        // Whether to bracket is the receiving application's decision, not this client's: an
        // application that never enabled the mode would read the wrapper as literal input.
        let bracketed = self
            .focused_run_id()
            .and_then(|run_id| self.screens.get(run_id))
            .is_some_and(PaneScreen::bracketed_paste);
        self.send_to_pane(encode_paste(&text, bracketed))
    }

    /// Asks the daemon for a fresh Dock-owned shell in the focused pane.
    fn respawn(&mut self) -> UiCommand {
        let Some(workspace) = self.workspace() else {
            self.error = Some("restart unavailable: create a workspace first".into());
            return UiCommand::None;
        };
        let workspace_id = workspace.workspace_id.clone();
        let pane_id = workspace.focused_pane_id.clone();
        self.error = None;
        UiCommand::Request(Box::new(Request::Workspace(WorkspaceRequest::Respawn {
            workspace_id,
            pane_id,
        })))
    }

    fn open_launch(&mut self) {
        self.error = None;
        self.launch_form = Some(LaunchForm {
            index: self.last_launch_profile.min(PROFILES.len() - 1),
            repository_mode: self.last_repository_mode && !self.repository_launches.is_empty(),
            confirming: false,
            query: String::new(),
        });
    }

    fn launch_key(&mut self, key: KeyEvent) -> UiCommand {
        let form = self.launch_form.as_mut().expect("launch form");
        match key.code {
            KeyCode::Esc => {
                self.launch_form = None;
                UiCommand::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                form.index = previous_matching(form.index, &form.query);
                form.confirming = false;
                UiCommand::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                form.index = next_matching(form.index, &form.query);
                form.confirming = false;
                UiCommand::None
            }
            KeyCode::Tab => {
                if self.repository_launches.is_empty() {
                    self.error = Some(
                        "repository mode unavailable: no verified repository/task/worktree option"
                            .into(),
                    );
                } else {
                    form.repository_mode = !form.repository_mode;
                    form.confirming = false;
                    self.error = None;
                }
                UiCommand::None
            }
            KeyCode::Char(character) if !form.confirming && !character.is_control() => {
                form.query.push(character);
                if let Some(index) = matching_profiles(&form.query).next() {
                    form.index = index;
                    self.error = None;
                } else {
                    self.error = Some(format!("no fixed provider matches ‘{}’", form.query));
                }
                UiCommand::None
            }
            KeyCode::Backspace if !form.confirming => {
                form.query.pop();
                if let Some(index) = matching_profiles(&form.query).next() {
                    form.index = index;
                    self.error = None;
                }
                UiCommand::None
            }
            KeyCode::Enter if !form.confirming => {
                if matching_profiles(&form.query).any(|index| index == form.index) {
                    form.confirming = true;
                    self.error = None;
                } else {
                    self.error = Some("launch unavailable: no provider matches the filter".into());
                }
                UiCommand::None
            }
            KeyCode::Enter => self.confirm_launch(),
            _ => UiCommand::None,
        }
    }

    fn confirm_launch(&mut self) -> UiCommand {
        let form = self.launch_form.clone().expect("launch form");
        let Some(workspace) = self.workspace() else {
            self.error = Some("cannot launch: create a workspace first".into());
            self.launch_form = None;
            return UiCommand::None;
        };
        let workspace_id = workspace.workspace_id.clone();
        let pane_id = workspace.focused_pane_id.clone();
        let run_id = self.next_unique_id("dock_ui");
        let profile = PROFILES[form.index].0;
        let id = AdapterId::from(profile);
        if !crate::adapter::builtin_available(&id) {
            self.error = Some(format!(
                "{} is unavailable: fixed executable not found",
                PROFILES[form.index].1
            ));
            return UiCommand::None;
        }
        self.last_launch_profile = form.index;
        self.last_repository_mode = form.repository_mode;
        self.launch_form = None;
        if !form.repository_mode {
            return UiCommand::Request(Box::new(Request::TerminalLaunch(TerminalLaunchRequest {
                workspace_id,
                pane_id,
                run_id,
                profile,
                runtime_directory: self.runtime_directory.clone(),
                arguments: Vec::new(),
                // Launched by hand rather than off a card, so there is no task to record.
                external_task_ref: String::new(),
            })));
        }
        let Some(option) = self.repository_launches.first() else {
            self.error = Some("repository dispatch is unavailable".into());
            return UiCommand::None;
        };
        UiCommand::Request(Box::new(Request::LaunchIntoPane(LaunchIntoPaneRequest {
            workspace_id,
            pane_id,
            dispatch: DispatchRequest {
                repository_root: self.repository_root.clone(),
                external_task_ref: option.task_ref.clone(),
                run_id,
                worktree: option.worktree.clone(),
                adapter: AdapterSelection {
                    id,
                    executable: None,
                    arguments: if profile == DashboardProfile::Fixture {
                        vec![
                            "-c".into(),
                            "printf 'Dock-owned fixture ready\\n'; sleep 30".into(),
                        ]
                    } else {
                        vec![]
                    },
                },
            },
        })))
    }

    fn render_launch_form(&mut self, frame: &mut Frame, area: Rect) {
        let form = self.launch_form.as_ref().expect("launch form");
        let width = area.width.min(58);
        let height = area.height.min(13);
        let popup = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        let target = self
            .workspace()
            .map(|workspace| format!("{}/{}", workspace.name, workspace.focused_pane_id))
            .unwrap_or_else(|| "unavailable (create workspace first)".into());
        let mut lines = vec![Line::from(format!(
            "Mode: {}  [Tab] toggle · Target: {}",
            if form.repository_mode {
                "repository-bound"
            } else {
                "unbound terminal"
            },
            target
        ))];
        self.launch_mode_area = Some(Rect::new(
            popup.x + 1,
            popup.y + 1,
            popup.width.saturating_sub(2),
            1,
        ));
        self.launch_profile_areas = (0..PROFILES.len())
            .map(|index| {
                Rect::new(
                    popup.x + 1,
                    popup.y + 2 + index as u16,
                    popup.width.saturating_sub(2),
                    1,
                )
            })
            .collect();
        for (index, (profile, label)) in PROFILES.iter().enumerate() {
            let available = crate::adapter::builtin_available(&AdapterId::from(*profile));
            let matches = profile_matches(index, &form.query);
            lines.push(Line::styled(
                format!(
                    "{} {} — {}",
                    if index == form.index && matches {
                        "›"
                    } else {
                        " "
                    },
                    label,
                    if available {
                        "available"
                    } else {
                        "unavailable: fixed executable not found"
                    }
                ),
                Style::default().fg(if !matches {
                    self.theme.surface
                } else if available {
                    self.theme.accent
                } else {
                    self.theme.muted
                }),
            ));
        }
        lines.push(Line::from(if form.confirming {
            format!(
                "REVIEW {} → {} · Enter launches · Esc cancels",
                PROFILES[form.index].1, target
            )
        } else {
            format!(
                "Filter: {}█ · type, ↑/↓/j/k select · Enter review · Esc cancels",
                form.query
            )
        }));
        self.launch_confirm_area = Some(Rect::new(
            popup.x + 1,
            popup.y + 2 + PROFILES.len() as u16,
            popup.width.saturating_sub(2),
            1,
        ));
        frame.render_widget(
            Paragraph::new(lines)
                .style(Style::default().fg(self.theme.text))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(Theme::border_type())
                        .border_style(Style::default().fg(self.theme.border_focused))
                        .title(" LAUNCH FIXED PROFILE "),
                ),
            popup,
        );
    }

    fn focus_next(&mut self, reverse: bool) -> UiCommand {
        let Some(workspace) = self.workspace() else {
            self.error = Some("focus unavailable: create a workspace first".into());
            return UiCommand::None;
        };
        let ids: Vec<_> = workspace.panes.keys().collect();
        let current = ids
            .iter()
            .position(|id| ***id == workspace.focused_pane_id)
            .unwrap_or(0);
        let next = if reverse {
            current
                .checked_sub(1)
                .unwrap_or(ids.len().saturating_sub(1))
        } else {
            (current + 1) % ids.len()
        };
        let workspace_id = workspace.workspace_id.clone();
        let pane_id = ids[next].to_string();
        self.layout.workspaces[self.workspace_index].focused_pane_id = pane_id.clone();
        self.error = None;
        UiCommand::Request(Box::new(Request::Workspace(WorkspaceRequest::Focus {
            workspace_id,
            pane_id,
        })))
    }

    /// Divides the focused pane, giving the new half the kind asked for.
    ///
    /// The local layout is updated before the request goes out, for the reason every other
    /// command here does it: the split is visible in the frame painted before the daemon is
    /// asked, and `refresh` is what the dashboard actually believes afterwards.
    fn split(&mut self, axis: SplitAxis, kind: PaneKind) -> UiCommand {
        let Some((workspace_id, pane_id)) = self.workspace().map(|workspace| {
            (
                workspace.workspace_id.clone(),
                workspace.focused_pane_id.clone(),
            )
        }) else {
            self.error = Some("split unavailable: create a workspace first".into());
            return UiCommand::None;
        };
        let new_pane_id = self.next_unique_id("pane");
        let workspace = &mut self.layout.workspaces[self.workspace_index];
        split_leaf(&mut workspace.root, &pane_id, new_pane_id.clone(), axis);
        workspace.panes.insert(
            new_pane_id.clone(),
            PaneLayout {
                pane_id: new_pane_id.clone(),
                name: match kind {
                    PaneKind::Terminal => new_pane_id.replace('_', " "),
                    PaneKind::Board => "board".into(),
                },
                run_id: None,
                runtime: PaneRuntime::Empty,
                kind,
            },
        );
        workspace.focused_pane_id = new_pane_id.clone();
        UiCommand::Request(Box::new(Request::Workspace(WorkspaceRequest::Split {
            workspace_id,
            pane_id,
            new_pane_id,
            axis,
            kind,
        })))
    }

    fn rename(&mut self) -> UiCommand {
        let Some(workspace) = self.workspace() else {
            self.error = Some("rename unavailable: create a workspace first".into());
            return UiCommand::None;
        };
        self.rename_form = Some((
            RenameTarget::Pane,
            workspace.panes[&workspace.focused_pane_id].name.clone(),
        ));
        self.error = None;
        UiCommand::None
    }

    /// Opens the rename form on the visible workspace rather than the focused pane.
    fn rename_workspace(&mut self) -> UiCommand {
        let Some(workspace) = self.workspace() else {
            self.error = Some("rename unavailable: create a workspace first".into());
            return UiCommand::None;
        };
        self.rename_form = Some((RenameTarget::Workspace, workspace.name.clone()));
        self.error = None;
        UiCommand::None
    }

    fn close(&mut self) -> UiCommand {
        let Some(workspace) = self.workspace() else {
            self.error = Some("close unavailable: create a workspace first".into());
            return UiCommand::None;
        };
        UiCommand::Request(Box::new(Request::Workspace(WorkspaceRequest::Close {
            workspace_id: workspace.workspace_id.clone(),
            pane_id: workspace.focused_pane_id.clone(),
        })))
    }

    /// Closes the visible workspace, on the second click rather than the first.
    ///
    /// The daemon has no "close workspace" operation and does not need one: it drops a workspace
    /// once its last pane is gone, so this closes every pane and lets that rule do the work.
    /// Nothing is repainted optimistically, unlike rename and split — the pane close this is
    /// built from does not either, and inventing a local guess about which panes survived a
    /// batch the daemon has not answered yet is exactly the disagreement `refresh` exists to
    /// prevent.
    fn close_workspace(&mut self) -> UiCommand {
        let Some(workspace) = self.workspace() else {
            self.error = Some("close unavailable: create a workspace first".into());
            return UiCommand::None;
        };
        let workspace_id = workspace.workspace_id.clone();
        // Asked only when there is something to lose. A prompt on every empty workspace is one
        // that gets dismissed by reflex, and reflex is exactly what has to fail on the workspace
        // that is holding three agents mid-task.
        if self.running_agents_here() == 0 {
            return self.close_workspace_now();
        }
        if self.close_workspace_armed.as_deref() != Some(workspace_id.as_str()) {
            self.close_workspace_armed = Some(workspace_id);
            self.error = None;
            return UiCommand::None;
        }
        self.confirm_close_workspace()
    }

    /// How many agents are running in the visible workspace.
    ///
    /// Counted from detected agents rather than live processes, because every pane holds a shell
    /// from the moment it exists, so "has a process" is true of all of them and would make the
    /// question meaningless. The cost is honest and worth stating: a long command running in a
    /// plain shell is not an agent and does not raise the prompt.
    fn running_agents_here(&self) -> usize {
        let Some(workspace) = self.workspace() else {
            return 0;
        };
        workspace
            .panes
            .values()
            .filter_map(|pane| pane.run_id.as_deref())
            .filter(|run_id| {
                self.agents
                    .get(*run_id)
                    .is_some_and(|(kind, _)| kind.is_some())
            })
            .count()
    }

    /// Closes the visible workspace, having been answered rather than merely asked.
    fn confirm_close_workspace(&mut self) -> UiCommand {
        self.close_workspace_now()
    }

    fn close_workspace_now(&mut self) -> UiCommand {
        let Some(workspace) = self.workspace() else {
            self.error = Some("close unavailable: create a workspace first".into());
            return UiCommand::None;
        };
        let workspace_id = workspace.workspace_id.clone();
        let requests = workspace
            .panes
            .keys()
            .map(|pane_id| {
                Request::Workspace(WorkspaceRequest::Close {
                    workspace_id: workspace_id.clone(),
                    pane_id: pane_id.clone(),
                })
            })
            .collect();
        self.close_workspace_armed = None;
        self.error = None;
        UiCommand::Requests(requests)
    }

    /// Withdraws a pending workspace close. Arming is a question about the workspace on screen,
    /// so anything that changes which workspace that is, or that answers with something other
    /// than a second click on the same control, has to take the question back.
    fn disarm_workspace_close(&mut self) {
        self.close_workspace_armed = None;
    }

    fn rename_key(&mut self, key: KeyEvent) -> UiCommand {
        match key.code {
            KeyCode::Esc => {
                self.rename_form = None;
                self.error = None;
                UiCommand::None
            }
            KeyCode::Backspace => {
                self.rename_form.as_mut().expect("rename form").1.pop();
                UiCommand::None
            }
            KeyCode::Char(character) if !character.is_control() => {
                let (_, value) = self.rename_form.as_mut().expect("rename form");
                if value.chars().count() < 80 {
                    value.push(character);
                }
                UiCommand::None
            }
            KeyCode::Enter => {
                let (target, value) = self.rename_form.as_ref().expect("rename form");
                let (target, name) = (*target, value.trim().to_owned());
                if name.is_empty() {
                    self.error = Some("rename unavailable: name cannot be empty".into());
                    return UiCommand::None;
                }
                let workspace = self
                    .workspace()
                    .expect("workspace retained while form open");
                let workspace_id = workspace.workspace_id.clone();
                let pane_id = workspace.focused_pane_id.clone();
                // Painted locally before the request, like every other command here, so the new
                // name is on screen before the daemon has answered.
                let pane_id = match target {
                    RenameTarget::Pane => {
                        self.layout.workspaces[self.workspace_index]
                            .panes
                            .get_mut(&pane_id)
                            .expect("focused pane")
                            .name = name.clone();
                        Some(pane_id)
                    }
                    RenameTarget::Workspace => {
                        self.layout.workspaces[self.workspace_index].name = name.clone();
                        None
                    }
                };
                self.rename_form = None;
                self.error = None;
                UiCommand::Request(Box::new(Request::Workspace(WorkspaceRequest::Rename {
                    workspace_id,
                    pane_id,
                    name,
                })))
            }
            _ => UiCommand::None,
        }
    }

    fn resize_keyboard(&mut self, delta: i16) -> UiCommand {
        let Some(workspace) = self.workspace() else {
            self.error = Some("resize unavailable: create a split workspace first".into());
            return UiCommand::None;
        };
        let workspace_id = workspace.workspace_id.clone();
        let pane_id = workspace.focused_pane_id.clone();
        let Some(ratio) = adjust_parent_ratio(
            &mut self.layout.workspaces[self.workspace_index].root,
            &pane_id,
            delta,
        ) else {
            self.error = Some("resize unavailable: focused pane has no split divider".into());
            return UiCommand::None;
        };
        self.error = None;
        UiCommand::Request(Box::new(Request::Workspace(WorkspaceRequest::Resize {
            workspace_id,
            pane_id,
            ratio_milli: ratio,
        })))
    }

    pub fn mouse(&mut self, event: MouseEvent) -> UiCommand {
        // An open picker is modal: clicking a row takes it, and clicking anywhere else is
        // swallowed rather than reaching the panes underneath, which are not what is being
        // pointed at while an overlay covers them.
        if self.picker.is_some() {
            if event.kind == MouseEventKind::Down(MouseButton::Left)
                && let Some(row) = self
                    .picker_row_areas
                    .iter()
                    .position(|area| contains(*area, event.column, event.row))
            {
                let taken = self.picker.as_ref().and_then(|(purpose, picker)| {
                    picker
                        .visible()
                        .nth(row)
                        .map(|(item, _)| (*purpose, item.key.clone()))
                });
                self.picker = None;
                if let Some((purpose, key)) = taken {
                    return self.take_picked(purpose, &key);
                }
            }
            return UiCommand::None;
        }
        if self.launch_form.is_some() {
            if event.kind == MouseEventKind::Down(MouseButton::Left) {
                if self
                    .launch_mode_area
                    .is_some_and(|area| contains(area, event.column, event.row))
                {
                    if self.repository_launches.is_empty() {
                        self.error = Some("repository mode unavailable: no verified repository/task/worktree option".into());
                    } else {
                        let form = self.launch_form.as_mut().expect("launch form");
                        form.repository_mode = !form.repository_mode;
                        form.confirming = false;
                        self.error = None;
                    }
                    return UiCommand::None;
                }
                if let Some(index) = self
                    .launch_profile_areas
                    .iter()
                    .position(|area| contains(*area, event.column, event.row))
                {
                    let form = self.launch_form.as_mut().expect("launch form");
                    form.index = index;
                    form.confirming = false;
                    return UiCommand::None;
                }
                if self
                    .launch_confirm_area
                    .is_some_and(|area| contains(area, event.column, event.row))
                {
                    if self
                        .launch_form
                        .as_ref()
                        .is_some_and(|form| form.confirming)
                    {
                        return self.confirm_launch();
                    }
                    self.launch_form.as_mut().expect("launch form").confirming = true;
                }
            }
            return UiCommand::None;
        }
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                // A fresh press ends any previous gesture, whatever swallowed its release.
                // Without this a stale arming would hijack the next divider drag, and a ratio
                // left over from a divider drag whose release never arrived — a window resize
                // clears `dragging` mid-gesture — would be sent on the release of whatever
                // gesture came next.
                self.pane_drag = None;
                self.pending_divider_resize = None;
                // Tab-strip chrome first: these sit beside the tabs, so testing tabs first would
                // swallow clicks that landed on the controls.
                if self
                    .new_workspace_area
                    .is_some_and(|area| contains(area, event.column, event.row))
                {
                    return self.run_command(PaneCommand::NewWorkspace);
                }
                if self
                    .confirm_close_workspace_area
                    .is_some_and(|area| contains(area, event.column, event.row))
                {
                    return self.confirm_close_workspace();
                }
                if self
                    .close_workspace_area
                    .is_some_and(|area| contains(area, event.column, event.row))
                {
                    // Armed, this cell is the cancel. It keeps the position the close control
                    // had, so the second press of a double-click lands on "no" rather than
                    // destroying the workspace the first press only meant to ask about.
                    if self.close_workspace_armed.is_some() {
                        self.disarm_workspace_close();
                        return UiCommand::None;
                    }
                    return self.close_workspace();
                }
                // Every other press is an answer of "no" to a pending workspace close. A primed
                // destructive control that survives the user going off to do something else is
                // a trap: the click that arms it and the click that fires it must be adjacent.
                self.disarm_workspace_close();
                if self
                    .rename_workspace_area
                    .is_some_and(|area| contains(area, event.column, event.row))
                {
                    return self.rename_workspace();
                }
                // The scroll markers sit on the same row as the tabs, so testing tabs first
                // would swallow a click meant for one of them.
                if self
                    .tab_scroll_left_area
                    .is_some_and(|area| contains(area, event.column, event.row))
                {
                    self.tab_scroll = self.tab_scroll.saturating_sub(1);
                    return UiCommand::None;
                }
                if self
                    .tab_scroll_right_area
                    .is_some_and(|area| contains(area, event.column, event.row))
                {
                    self.tab_scroll = self.tab_scroll.saturating_add(1);
                    return UiCommand::None;
                }
                if let Some(workspace_id) = self
                    .tab_areas
                    .iter()
                    .find(|(_, area)| contains(*area, event.column, event.row))
                    .map(|(workspace_id, _)| workspace_id.clone())
                {
                    return self.take_picked(PickerPurpose::Workspace, &workspace_id);
                }
                // Pane controls sit on the border, which is outside the pane body, so they can be
                // tested before the body hit-test without stealing a click meant for the pane.
                if let Some(control) = self
                    .pane_control_areas
                    .iter()
                    .find(|(_, area)| contains(*area, event.column, event.row))
                    .map(|(control, _)| *control)
                {
                    return self.run_command(match control {
                        PaneControl::SplitHorizontal => PaneCommand::Split(SplitAxis::Horizontal),
                        PaneControl::SplitVertical => PaneCommand::Split(SplitAxis::Vertical),
                        PaneControl::Rename => PaneCommand::Rename,
                        PaneControl::Close => PaneCommand::Close,
                    });
                }
                if let Some(command) = self
                    .quick_action_areas
                    .iter()
                    .find(|(_, area)| contains(*area, event.column, event.row))
                    .map(|(command, _)| *command)
                {
                    return self.run_command(command);
                }
                if self
                    .launch_area
                    .is_some_and(|area| contains(area, event.column, event.row))
                {
                    self.open_launch();
                    return UiCommand::LoadCatalog;
                }
                if let Some(divider) = self
                    .dividers
                    .iter()
                    .find(|divider| contains(divider.area, event.column, event.row))
                {
                    self.dragging = Some(DragTarget {
                        pane_id: divider.pane_id.clone(),
                        axis: divider.axis,
                    });
                    return UiCommand::None;
                }
                let pane = self
                    .pane_areas
                    .iter()
                    .find(|(_, area)| contains(**area, event.column, event.row))
                    .map(|(id, _)| id.clone());
                let Some((workspace_id, pane_id)) = self
                    .workspace()
                    .and_then(|w| pane.map(|p| (w.workspace_id.clone(), p)))
                else {
                    return UiCommand::None;
                };
                // A press inside a pane body only *arms* a selection; copy mode is entered
                // on the first drag. Entering it here would mean every click that focuses a
                // pane also puts the keyboard into a mode that swallows it.
                let armed = self
                    .workspace()
                    .and_then(|workspace| workspace.panes.get(&pane_id))
                    .and_then(|pane| pane.run_id.clone())
                    .zip(self.pane_inner_areas.get(&pane_id).copied())
                    .and_then(|(run_id, inner)| {
                        grid_cell(inner, event.column, event.row).map(|origin| PaneDrag {
                            run_id,
                            origin,
                            inner,
                            selected: false,
                        })
                    });
                self.pane_drag = armed;
                let clicks = self.count_click(event.column, event.row);
                if clicks > 1 {
                    self.select_by_click(clicks);
                }
                // Read before the assignment below, which would otherwise make every pane look
                // as though it had just been focused.
                let already_focused = self
                    .workspace()
                    .is_some_and(|workspace| workspace.focused_pane_id == pane_id);
                if already_focused {
                    // Focusing the pane that already has focus cost three blocking daemon round
                    // trips — this request, then `refresh`'s `Workspace(Inspect)` and `Inspect` —
                    // on the press that begins every selection, and the daemon's inspect may
                    // shell out to `ps` on a cache miss. That hitch was the first thing a user
                    // felt when they went to select text, and it bought nothing: the answer was
                    // always the focus the dashboard already had.
                    return UiCommand::None;
                }
                self.layout.workspaces[self.workspace_index].focused_pane_id = pane_id.clone();
                UiCommand::Send(Box::new(Request::Workspace(WorkspaceRequest::Focus {
                    workspace_id,
                    pane_id,
                })))
            }
            MouseEventKind::Down(MouseButton::Middle)
            | MouseEventKind::Down(MouseButton::Right) => {
                self.paste_last_copied(event.column, event.row)
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                // A press landed either on a divider or in a pane body, never both, so this
                // reads the pane gesture first and leaves the divider path below untouched.
                if let Some(drag) = self.pane_drag.clone() {
                    self.drag_selection(&drag, event.column, event.row);
                    if let Some(armed) = self.pane_drag.as_mut() {
                        armed.selected = true;
                    }
                    return UiCommand::None;
                }
                let Some(target) = self.dragging.as_ref() else {
                    return UiCommand::None;
                };
                let Some(divider) = self.dividers.iter().find(|divider| {
                    divider.pane_id == target.pane_id && divider.axis == target.axis
                }) else {
                    self.dragging = None;
                    return UiCommand::None;
                };
                let ratio = drag_ratio(divider, event.column, event.row);
                let pane_id = divider.pane_id.clone();
                let Some(workspace) = self.workspace() else {
                    return UiCommand::None;
                };
                let workspace_id = workspace.workspace_id.clone();
                set_parent_ratio(
                    &mut self.layout.workspaces[self.workspace_index].root,
                    &pane_id,
                    ratio,
                );
                // Held rather than sent: the layout above already moved locally, so the
                // divider tracks the pointer, and the daemon hears one ratio — the one the
                // pointer finished on — when the button comes up.
                self.pending_divider_resize = Some((workspace_id, pane_id, ratio));
                UiCommand::None
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.dragging = None;
                // Auto-copy on release, which is what every modern terminal does and what a
                // user coming from iTerm2, Ghostty or WezTerm expects: a selection that has to
                // be confirmed with a second keystroke is a selection most people never copy.
                // `y` still works, and still leaves copy mode.
                let selected = self.pane_drag.take().is_some_and(|drag| drag.selected);
                if selected {
                    self.copy_pointer_selection();
                }
                match self.pending_divider_resize.take() {
                    Some((workspace_id, pane_id, ratio_milli)) => {
                        UiCommand::Request(Box::new(Request::Workspace(WorkspaceRequest::Resize {
                            workspace_id,
                            pane_id,
                            ratio_milli,
                        })))
                    }
                    None => UiCommand::None,
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                // The strip is tested first and returns early: it shares this arm with pane
                // scrolling below, and a notch over the tabs must move the strip rather than
                // whatever pane happens to sit under the same column further down the canvas.
                if self
                    .tab_strip_area
                    .is_some_and(|area| contains(area, event.column, event.row))
                {
                    if event.kind == MouseEventKind::ScrollDown {
                        self.tab_scroll = self.tab_scroll.saturating_add(1);
                    } else {
                        self.tab_scroll = self.tab_scroll.saturating_sub(1);
                    }
                    return UiCommand::None;
                }
                // Three rows per notch matches what terminals send for a single wheel click.
                let back = event.kind == MouseEventKind::ScrollUp;
                let delta = if back { 3 } else { -3 };
                let run_id = self
                    .pane_areas
                    .iter()
                    .find(|(_, area)| contains(**area, event.column, event.row))
                    .and_then(|(pane_id, _)| self.workspace()?.panes.get(pane_id))
                    .and_then(|pane| pane.run_id.clone());
                if let Some(run_id) = run_id {
                    self.scroll_pane(&run_id, delta);
                    // Only a notch *back* can want output older than the pane holds; a notch
                    // toward live output moves through rows it already has. Asking on both put
                    // a two-megabyte round trip and a full parser rebuild on half of every
                    // wheel gesture, for history nobody was scrolling towards.
                    if back && let Some(request) = self.history_request_for(&run_id) {
                        return UiCommand::Request(Box::new(request));
                    }
                }
                UiCommand::None
            }
            _ => UiCommand::None,
        }
    }

    fn next_unique_id(&mut self, prefix: &str) -> String {
        self.sequence = self.sequence.max(
            self.layout
                .workspaces
                .iter()
                .flat_map(|workspace| {
                    std::iter::once(workspace.workspace_id.as_str())
                        .chain(workspace.panes.keys().map(String::as_str))
                })
                .filter_map(|id| id.rsplit_once('_')?.1.parse::<u64>().ok())
                .max()
                .unwrap_or(0),
        );
        loop {
            self.sequence = self
                .sequence
                .checked_add(1)
                .expect("generated ID space exhausted");
            let candidate = format!("{prefix}_{}", self.sequence);
            let collision = self.layout.workspaces.iter().any(|workspace| {
                workspace.workspace_id == candidate || workspace.panes.contains_key(&candidate)
            });
            if !collision {
                return candidate;
            }
        }
    }
}

fn matching_profiles(query: &str) -> impl Iterator<Item = usize> + '_ {
    (0..PROFILES.len()).filter(move |index| profile_matches(*index, query))
}

fn profile_matches(index: usize, query: &str) -> bool {
    PROFILES[index]
        .1
        .to_ascii_lowercase()
        .contains(&query.to_ascii_lowercase())
}

fn next_matching(current: usize, query: &str) -> usize {
    (1..=PROFILES.len())
        .map(|offset| (current + offset) % PROFILES.len())
        .find(|index| profile_matches(*index, query))
        .unwrap_or(current)
}

fn previous_matching(current: usize, query: &str) -> usize {
    (1..=PROFILES.len())
        .map(|offset| (current + PROFILES.len() - offset) % PROFILES.len())
        .find(|index| profile_matches(*index, query))
        .unwrap_or(current)
}

fn split_leaf(node: &mut LayoutNode, pane_id: &str, new_pane_id: String, axis: SplitAxis) -> bool {
    match node {
        LayoutNode::Pane { pane_id: id } if id == pane_id => {
            let old = id.clone();
            *node = LayoutNode::Split {
                axis,
                ratio_milli: 500,
                first: Box::new(LayoutNode::Pane { pane_id: old }),
                second: Box::new(LayoutNode::Pane {
                    pane_id: new_pane_id,
                }),
            };
            true
        }
        LayoutNode::Pane { .. } => false,
        LayoutNode::Split { first, second, .. } => {
            split_leaf(first, pane_id, new_pane_id.clone(), axis)
                || split_leaf(second, pane_id, new_pane_id, axis)
        }
    }
}

fn adjust_parent_ratio(node: &mut LayoutNode, pane_id: &str, delta: i16) -> Option<u16> {
    match node {
        LayoutNode::Pane { .. } => None,
        LayoutNode::Split {
            ratio_milli,
            first,
            second,
            ..
        } => {
            if first_leaf(second) == pane_id {
                *ratio_milli = (i32::from(*ratio_milli) + i32::from(delta)).clamp(100, 900) as u16;
                Some(*ratio_milli)
            } else {
                adjust_parent_ratio(first, pane_id, delta)
                    .or_else(|| adjust_parent_ratio(second, pane_id, delta))
            }
        }
    }
}

fn set_parent_ratio(node: &mut LayoutNode, pane_id: &str, ratio: u16) -> bool {
    match node {
        LayoutNode::Pane { .. } => false,
        LayoutNode::Split {
            ratio_milli,
            first,
            second,
            ..
        } => {
            if first_leaf(second) == pane_id {
                *ratio_milli = ratio;
                true
            } else {
                set_parent_ratio(first, pane_id, ratio) || set_parent_ratio(second, pane_id, ratio)
            }
        }
    }
}

fn split_rect(area: Rect, axis: SplitAxis, ratio: u16) -> (Rect, Rect, Rect) {
    match axis {
        SplitAxis::Vertical => {
            let available = area.width.saturating_sub(1);
            let first = ((u32::from(available) * u32::from(ratio)) / 1000) as u16;
            (
                Rect::new(area.x, area.y, first, area.height),
                Rect::new(area.x + first, area.y, 1, area.height),
                Rect::new(area.x + first + 1, area.y, available - first, area.height),
            )
        }
        SplitAxis::Horizontal => {
            let available = area.height.saturating_sub(1);
            let first = ((u32::from(available) * u32::from(ratio)) / 1000) as u16;
            (
                Rect::new(area.x, area.y, area.width, first),
                Rect::new(area.x, area.y + first, area.width, 1),
                Rect::new(area.x, area.y + first + 1, area.width, available - first),
            )
        }
    }
}

fn first_leaf(node: &LayoutNode) -> &str {
    match node {
        LayoutNode::Pane { pane_id } => pane_id,
        LayoutNode::Split { first, .. } => first_leaf(first),
    }
}
/// One row of the agent roster: how badly the agent wants attention, what it is, the task it is
/// on, and the workspace it is in. Borrowed out of the dashboard rather than copied, so an entry
/// the sidebar turns out to have no room for costs nothing beyond the comparison that sorted it.
type RosterEntry<'a> = (AgentState, &'a str, Option<Cow<'a, str>>, Option<&'a str>);

/// The sidebar's rows as they are built: numbered as if all of them existed, kept only while
/// they still fit inside the sidebar.
///
/// The sidebar used to build a `Line` for every workspace and every agent and hand the whole
/// list to `Paragraph`, which draws the first `area.height` of them and drops the rest. On a
/// busy runtime that was most of the sidebar's work — and most of a frame's allocations —
/// spent formatting rows nobody could see, because the roster spans every workspace and costs
/// up to two lines per agent. Every row is still *numbered* as though it had been built, since
/// [`clickable_row`] addresses a row by its index and a menu whose rectangles slid up a row
/// would send clicks to the wrong action.
struct SidebarRows {
    lines: Vec<Line<'static>>,
    height: usize,
    /// The index the next row will take, which keeps counting past `height` so that
    /// [`clickable_row`] keeps answering `None` for everything below the fold.
    next: usize,
}

impl SidebarRows {
    fn new(height: u16) -> Self {
        let height = usize::from(height);
        Self {
            lines: Vec::with_capacity(height),
            height,
            next: 0,
        }
    }

    /// Adds the next row, building it only when there is somewhere to draw it. Taking a closure
    /// rather than a `Line` is the whole point: an invisible row must cost nothing, and a row
    /// passed by value has already paid for every `format!` inside it.
    fn push(&mut self, line: impl FnOnce() -> Line<'static>) {
        if self.next < self.height {
            self.lines.push(line());
        }
        self.next += 1;
    }

    /// The index of the row just added, for [`clickable_row`].
    fn last(&self) -> usize {
        self.next.saturating_sub(1)
    }

    /// Whether the next row would land anywhere visible. Asked before formatting a whole roster
    /// entry, which is more than one row's worth of work.
    fn has_room(&self) -> bool {
        self.next < self.height
    }

    fn into_lines(self) -> Vec<Line<'static>> {
        self.lines
    }
}

/// The task the daemon itself bound to a run, if it bound one. A blank reference means unbound
/// rather than a task whose name happens to be empty, and the single lookup and the batch index
/// have to agree about that or the roster and the pane title would disagree on screen.
/// A card narrower than this cannot hold a whole liveness line, so it stops trying.
///
/// Five columns across a hundred-wide terminal leave about fourteen cells, where
/// `claude · a · needs you · 0 queued` ellipsises into the middle of the one word worth reading.
/// Below this the second line is the state word alone: the glyph is the glance, the word is the
/// fact, and the fact is what survives.
const NARROW_CARD: usize = 20;

/// And narrower still than *this*, a card has no second line at all.
///
/// Sized to `  not running`, which is the longest of the short forms. Five columns across an
/// eighty-wide terminal, with the sidebar taking its twenty-eight, leave nine cells — and a card
/// reading `nee…` where it means "needs you" is worse than a card that says nothing and leaves
/// the glyph and its colour to carry the state, which they are there to do.
const ONE_LINE_CARD: usize = 13;

/// The board's columns and the cards in them, over whatever rectangle it is handed.
///
/// A free function rather than a method because it is the one thing the board overlay and the
/// Board pane genuinely share: the overlay hands it the inside of a popup, the pane hands it
/// everything above its footer, and neither has a second opinion about how a card looks. Two
/// rows go to the heading and its rule; everything below is cards.
///
/// The columns come from the view rather than from `board::STATUSES`, which is what makes a
/// `needs-input` card visible at all: the constant does not know that status, and every site that
/// resolved columns through it drew the card into no column and let `<`/`>` walk straight past it.
///
/// `live` is what is running, joined to the cards by the reference the daemon recorded when each
/// was dispatched. That join is the whole reason a board shows what is actually happening without
/// inventing a sixth column — and it is display-only: an entry is derived and vanishes when its
/// run does, whereas a status write is durable and would outlive whatever misread produced it.
fn render_board_columns(
    frame: &mut Frame,
    theme: &Theme,
    view: &BoardView,
    area: Rect,
    live: &BoardLive<'_>,
    cursor: (usize, usize),
) {
    let statuses = view.statuses();
    if statuses.is_empty() || area.width < statuses.len() as u16 || area.height < 3 {
        return;
    }
    // A board with nothing on it drew five headings reading `· 0` above four dashes and filled
    // the pane with them, which reads as broken rather than as empty. A board with an agent on it
    // is not empty whatever its files say — that agent is an entry in `ACTIVE`, which is the
    // whole point of this pass, so the grid is drawn for it.
    if view.tasks().is_empty() && live.runs.is_empty() {
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled("  no tasks on this board", Style::default().fg(theme.muted)),
                Line::from(""),
                Line::styled(
                    "  Ctrl+B k opens it · n adds a task",
                    Style::default().fg(theme.muted),
                ),
            ]),
            area,
        );
        return;
    }
    let column_width = area.width / statuses.len() as u16;
    let width = usize::from(column_width.saturating_sub(1));
    let card_rows = usize::from(area.height.saturating_sub(2));

    for (index, status) in statuses.iter().enumerate() {
        let x = area.x + column_width * index as u16;
        let active = index == cursor.0;
        // A column taller than the space scrolls to keep the cursor visible, rather than hiding
        // the selected card behind the bottom edge — so the selected row is passed down to
        // whichever of the two card shapes this column draws.
        let selected = active.then_some(cursor.1);
        let (count, lines) = if status == ACTIVE_STATUS {
            let entries = active_entries(view, live);
            (
                entries.len(),
                active_lines(theme, &entries, width, card_rows, selected),
            )
        } else {
            let cards = view.cards(status);
            (
                cards.len(),
                card_lines(theme, &cards, live, width, card_rows, selected),
            )
        };
        // A rule under each heading gives the columns edges without spending a column of
        // width on borders, which at five columns would cost a fifth of the card text.
        frame.render_widget(
            Paragraph::new(Line::styled(
                "─".repeat(usize::from(column_width.saturating_sub(2))),
                Style::default().fg(if active { theme.accent } else { theme.border }),
            )),
            Rect::new(x, area.y + 1, column_width.saturating_sub(1), 1),
        );
        frame.render_widget(
            Paragraph::new(Line::styled(
                ellipsise(&format!("{} · {count}", column_heading(status)), width),
                Style::default()
                    .fg(if active { theme.accent } else { theme.muted })
                    .add_modifier(if active {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            )),
            Rect::new(x, area.y, column_width.saturating_sub(1), 1),
        );
        // One paragraph for the whole column rather than one per card. Each card used to be its
        // own widget over its own one-row rectangle, which put a ratatui render — and the dozen
        // allocations inside it — behind every card on the board, on a path that repaints at
        // 60fps.
        frame.render_widget(
            Paragraph::new(lines),
            Rect::new(
                x,
                area.y + 2,
                column_width.saturating_sub(1),
                area.bottom().saturating_sub(area.y + 2),
            ),
        );
    }
}

/// What a column is called on screen.
///
/// `in-progress` is what every task file says, what `board::STATUSES` holds and what `<` and `>`
/// move a card between; none of that changes. `ACTIVE` is what the column *holds*, which is that
/// plus every live agent with no card in it — and "in progress" is not what a hand-launched agent
/// is.
fn column_heading(status: &str) -> Cow<'_, str> {
    if status == ACTIVE_STATUS {
        Cow::Borrowed("ACTIVE")
    } else {
        Cow::Owned(status.to_uppercase())
    }
}

/// Everything a column can put the cursor on, in the order it draws them.
///
/// The one place that decides what `j` and `k` walk through, so the cursor cannot disagree with
/// the grid about how many things are in a column or what order they are in.
fn column_targets(view: &BoardView, live: &BoardLive<'_>, column: usize) -> Vec<BoardTarget> {
    let Some(status) = view.statuses().get(column) else {
        return Vec::new();
    };
    if status == ACTIVE_STATUS {
        active_entries(view, live)
            .into_iter()
            .map(ActiveEntry::target)
            .collect()
    } else {
        view.cards(status)
            .into_iter()
            .map(|task| BoardTarget::Card(task.id))
            .collect()
    }
}

/// The one-line cards of every column that is not `ACTIVE`.
///
/// One line, not two. A backlog card has nothing to say on a second line, and vertical room is
/// what limits how many cards a person can see at once — so the asymmetry is itself the
/// information: a two-line card means this one is live.
fn card_lines<'a>(
    theme: &Theme,
    cards: &[&'a BoardTask],
    live: &BoardLive<'_>,
    width: usize,
    rows: usize,
    selected: Option<usize>,
) -> Vec<Line<'a>> {
    if cards.is_empty() {
        return vec![Line::styled("  —", Style::default().fg(theme.border))];
    }
    let first = scroll_to(selected, rows, 1);
    let mut lines = Vec::with_capacity(rows.min(cards.len()));
    for (row, task) in cards.iter().skip(first).take(rows).enumerate() {
        let here = selected == Some(first + row);
        let style = card_style(theme, here, true);
        let marker = if here { "›" } else { " " };
        // The badge is its own span so it keeps the state's colour against a selected card's
        // inverted background, where a single styled line would have lost it.
        lines.push(match live.by_task.get(&task.id) {
            Some(run) => Line::from(vec![
                Span::styled(format!("{marker} #{} ", task.id), style),
                Span::styled(
                    run.state.glyph().to_string(),
                    if here {
                        style
                    } else {
                        Style::default().fg(theme.agent(run.state))
                    },
                ),
                Span::styled(
                    ellipsise(
                        &format!(" {}", task.title),
                        width.saturating_sub(marker.len() + 3 + task.id.to_string().len()),
                    ),
                    style,
                ),
            ]),
            None => Line::styled(
                ellipsise(&format!("{marker} #{} {}", task.id, task.title), width),
                style,
            ),
        });
    }
    lines
}

/// The two-line entries of the `ACTIVE` column: identity above, liveness below.
///
/// Two lines because there are two questions — what is this, and what is happening to it — and
/// the second one only has an answer here. The glyph and the word both stay: a coloured dot says
/// that something is true of this agent without saying what, and "needs you" is the one state
/// worth crossing the room for. The same rule the sidebar roster follows.
fn active_lines(
    theme: &Theme,
    entries: &[ActiveEntry<'_>],
    width: usize,
    rows: usize,
    selected: Option<usize>,
) -> Vec<Line<'static>> {
    use std::fmt::Write as _;

    if entries.is_empty() {
        return vec![Line::styled("  —", Style::default().fg(theme.border))];
    }
    let per = if width < ONE_LINE_CARD { 1 } else { 2 };
    let visible = (rows / per).max(1);
    let first = scroll_to(selected, rows, per);
    let mut lines = Vec::with_capacity(visible * per);
    for (row, entry) in entries.iter().skip(first).take(visible).enumerate() {
        let here = selected == Some(first + row);
        let run = entry.run();
        // A dispatched card whose agent has gone is dimmed rather than hidden. It is precisely
        // the card a person has forgotten about, and hiding it would be the board agreeing.
        let style = card_style(theme, here, run.is_some());
        let live_style = match (here, run) {
            (true, _) => style,
            (false, Some(run)) => Style::default().fg(theme.agent(run.state)),
            (false, None) => Style::default().fg(theme.muted),
        };
        let mut identity = String::with_capacity(width + 8);
        match *entry {
            ActiveEntry::Card(task, _) => {
                let _ = write!(identity, " #{} {}", task.id, task.title);
            }
            // No card to name it by, so the agent names itself. What it is instead of a card is
            // on the line below, where a card id would have been.
            ActiveEntry::Loose(run) => {
                identity.push(' ');
                identity.push_str(run.agent.label());
            }
        }
        ellipsise_in_place(&mut identity, width.saturating_sub(2));
        lines.push(Line::from(vec![
            Span::styled(
                format!(
                    "{}{}",
                    if here { "›" } else { " " },
                    run.map_or(' ', |run| run.state.glyph())
                ),
                live_style,
            ),
            Span::styled(identity, style),
        ]));

        if per == 1 {
            continue;
        }
        let mut liveness = String::with_capacity(width + 8);
        liveness.push_str("  ");
        match run {
            None => liveness.push_str("not running"),
            Some(run) if width < NARROW_CARD => liveness.push_str(run.state.label()),
            Some(run) => {
                match *entry {
                    ActiveEntry::Card(..) => liveness.push_str(run.agent.label()),
                    ActiveEntry::Loose(run) => write_card_reference(&mut liveness, run),
                }
                liveness.push_str(" · ");
                write_liveness(&mut liveness, run);
            }
        }
        ellipsise_in_place(&mut liveness, width);
        lines.push(Line::styled(liveness, live_style));
    }
    lines
}

/// Where a column starts drawing, so the cursor stays on screen in a column taller than the
/// space. `per` is how many rows one entry costs, which is where the two card shapes differ.
fn scroll_to(selected: Option<usize>, rows: usize, per: usize) -> usize {
    let visible = (rows / per).max(1);
    selected.map_or(0, |index| index.saturating_sub(visible.saturating_sub(1)))
}

/// How a card is painted: under the cursor, live, or dispatched and abandoned.
fn card_style(theme: &Theme, selected: bool, live: bool) -> Style {
    if selected {
        Style::default()
            .bg(theme.accent)
            .fg(theme.surface)
            .add_modifier(Modifier::BOLD)
    } else if live {
        Style::default().fg(theme.text)
    } else {
        Style::default().fg(theme.muted)
    }
}

/// What a live agent with no entry of its own is called where a card id would go.
fn write_card_reference(buffer: &mut String, run: &LiveRun<'_>) {
    use std::fmt::Write as _;

    match run.task_id {
        Some(id) => {
            let _ = write!(buffer, "#{id}");
        }
        // Said rather than punctuated. A bare dash here is correct and reads as "a value is
        // missing", when what it means is that this agent was launched by hand rather than
        // dispatched from a card, and so has no card to be linked to.
        None => buffer.push_str("no card"),
    }
}

/// What an agent is doing and what is waiting behind it: the tail of every liveness line,
/// whichever identity that line opened with.
fn write_liveness(buffer: &mut String, run: &LiveRun<'_>) {
    use std::fmt::Write as _;

    // The queue depth is always drawn, including zero. A board that mentions a queue only when
    // it has something in it cannot be used to check that a queue is empty, which is the question
    // somebody about to arm a pane is actually asking.
    let _ = write!(
        buffer,
        "{} · {} · {} queued",
        run.pane_id,
        run.state.label(),
        run.queued
    );
    if run.auto_feed {
        buffer.push_str(" · armed");
    }
    if run.awaiting_ack {
        buffer.push_str(" · fed · waiting for the agent to pick it up");
    }
    // Only with something to hold. `holding_because` is set whenever auto-feed declined, and on
    // an empty queue it declines for the uninteresting reason that there was nothing to feed —
    // printing that on every entry would bury the one that is stuck.
    if run.queued > 0
        && let Some(why) = run.holding_because
    {
        let _ = write!(buffer, " · holding: {why}");
    }
}

/// What the board pane's footer says.
///
/// Three things, in the order they matter. The daemon-wide pause first, because an *armed* pane
/// that feeds nothing looks broken until something says the whole daemon is held. Then whatever
/// the cursor is on, in full — the card it sits under is thirty cells wide, and a stalled queue's
/// own sentence is seventy. Then the keys, published only on the pane that is taking them.
///
/// A free function beside the renderer rather than a method, because it is drawing rather than
/// state: everything it needs has already been resolved by the frame that calls it.
fn board_pane_footer(
    view: &BoardView,
    live: &BoardLive<'_>,
    (column, index): (usize, usize),
    paused: bool,
    focused: bool,
) -> String {
    use std::fmt::Write as _;

    let mut footer = String::with_capacity(128);
    if paused {
        footer.push_str("PAUSED · every queue is held");
    }
    if view.statuses().get(column).map(String::as_str) == Some(ACTIVE_STATUS)
        && let Some(entry) = active_entries(view, live).get(index).copied()
    {
        separate(&mut footer);
        match entry {
            ActiveEntry::Card(task, run) => {
                let _ = write!(footer, "#{}", task.id);
                match run {
                    Some(run) => {
                        // Named here even though the card above says it too: this line is read
                        // on its own, as the answer to "what is `a` about to arm".
                        let _ = write!(footer, " · {} · ", run.agent.label());
                        write_liveness(&mut footer, run);
                    }
                    // The card a person has forgotten about: dispatched, and whatever was
                    // working on it is gone. Said here as well as on the card, because this
                    // is the line that says what the cursor is about to act on.
                    None => footer.push_str(" · not running"),
                }
            }
            ActiveEntry::Loose(run) => {
                footer.push_str(run.agent.label());
                footer.push_str(" · ");
                write_liveness(&mut footer, run);
            }
        }
    }
    if focused {
        separate(&mut footer);
        footer.push_str("h/l column · j/k card · a arms auto-feed");
    }
    footer
}

/// Puts a separator between two things a line is joining, and nothing in front of the first.
fn separate(buffer: &mut String) {
    if !buffer.is_empty() {
        buffer.push_str(" · ");
    }
}

fn bound_task(run: &RuntimeSnapshot) -> Option<&str> {
    Some(run.external_task_ref.trim()).filter(|task| !task.is_empty())
}

/// The one-row rectangle for the sidebar line at `index`, or `None` when that line falls past
/// the bottom of the sidebar. A rectangle recorded off-screen would claim pointer coordinates
/// belonging to whatever is drawn there instead, so a row that was never rendered is not
/// clickable.
fn clickable_row(area: Rect, index: usize) -> Option<Rect> {
    let row = area.y.checked_add(u16::try_from(index).ok()?)?;
    (row < area.bottom()).then(|| Rect::new(area.x, row, area.width, 1))
}

/// The cell of a pane's grid under the pointer, or `None` if the pointer is on the border
/// or outside the pane entirely.
fn grid_cell(inner: Rect, column: u16, row: u16) -> Option<(u16, u16)> {
    contains(inner, column, row).then(|| (row - inner.y, column - inner.x))
}

/// The same conversion for a pointer that has been dragged past the pane's edge, clamped to
/// the nearest cell so the selection keeps growing instead of freezing at the boundary.
fn clamp_cell(inner: Rect, column: u16, row: u16) -> (u16, u16) {
    let last_row = inner.bottom().saturating_sub(1).max(inner.y);
    let last_column = inner.right().saturating_sub(1).max(inner.x);
    (
        row.clamp(inner.y, last_row) - inner.y,
        column.clamp(inner.x, last_column) - inner.x,
    )
}

/// A cell of the frame buffer addressed in a pane's grid coordinates.
fn cell_at(buffer: &mut Buffer, inner: Rect, row: u16, column: u16) -> Option<&mut Cell> {
    if row >= inner.height || column >= inner.width {
        return None;
    }
    buffer.cell_mut((inner.x + column, inner.y + row))
}

fn contains(area: Rect, x: u16, y: u16) -> bool {
    x >= area.x && x < area.right() && y >= area.y && y < area.bottom()
}
/// How far a viewport actually moved, as a signed row delta.
///
/// Positive means the visible rows moved *down* the screen, which is what going back into
/// history does: the offset grows and the cell that was at row `r` is now at row `r + moved`.
fn scrolled(before: usize, after: usize) -> i64 {
    i64::try_from(after).unwrap_or(i64::MAX) - i64::try_from(before).unwrap_or(i64::MAX)
}

/// Whether a character belongs to the same word as its neighbours.
///
/// Terminal content is not prose. What a person double-clicks in a pane is a path, an
/// identifier, a URL or a compiler's `file.rs:12:5`, so `/`, `.`, `-`, `_`, `~`, `+`, `@` and
/// `:` are *inside* a word rather than between two. That is iTerm2's default set plus `@` and
/// `:`, which is what makes `user@host`, `localhost:8080` and `src/main.rs:12` select whole
/// instead of in fragments. `,` and `;` are deliberately left out: in terminal output they end
/// a list item far more often than they appear inside one.
fn is_word_character(character: char) -> bool {
    character.is_alphanumeric() || "/.-_~+@:".contains(character)
}

/// The word under `column`, as inclusive grid columns, or `None` on blank padding.
///
/// A run of punctuation counts as a word of its own, so double-clicking the `->` between two
/// identifiers selects the arrow rather than silently selecting one of them.
///
/// Columns are character offsets rather than terminal cell widths, with exactly the caveat
/// [`crate::copy::find_matches`] documents: a row carrying CJK or emoji ahead of the click is
/// off by the count of those. Fixing it needs a width table this crate does not carry.
fn word_bounds(row: &str, column: u16) -> Option<(u16, u16)> {
    let characters: Vec<char> = row.chars().collect();
    let index = usize::from(column);
    let class = |position: usize| -> Option<bool> {
        characters
            .get(position)
            .filter(|character| !character.is_whitespace())
            .map(|character| is_word_character(*character))
    };
    let wanted = class(index)?;
    let mut first = index;
    while first > 0 && class(first - 1) == Some(wanted) {
        first -= 1;
    }
    let mut last = index;
    while class(last + 1) == Some(wanted) {
        last += 1;
    }
    Some((u16::try_from(first).ok()?, u16::try_from(last).ok()?))
}

/// The whole line, as inclusive grid columns, trimmed of the trailing blanks that are grid
/// padding rather than content. A blank row selects nothing.
fn line_bounds(row: &str) -> Option<(u16, u16)> {
    let last = row.trim_end().chars().count().checked_sub(1)?;
    Some((0, u16::try_from(last).ok()?))
}

fn drag_ratio(divider: &Divider, x: u16, y: u16) -> u16 {
    let (position, length, minimum) = match divider.axis {
        SplitAxis::Vertical => (
            x.saturating_sub(divider.container.x),
            divider.container.width.saturating_sub(1),
            MIN_PANE_WIDTH,
        ),
        SplitAxis::Horizontal => (
            y.saturating_sub(divider.container.y),
            divider.container.height.saturating_sub(1),
            MIN_PANE_HEIGHT,
        ),
    };
    let low = minimum.min(length / 2);
    let bounded = position.clamp(low, length.saturating_sub(low));
    if length == 0 {
        500
    } else {
        ((u32::from(bounded) * 1000) / u32::from(length)) as u16
    }
}
/// [`ellipsise`] for a string that was just built, cutting it in place rather than copying it.
///
/// The runs lane assembles a line per agent per frame and then has to fit it to the pane, and
/// doing that with `ellipsise` means every row is allocated twice — once to build and once to
/// trim. This trims the buffer that is already in hand, which is one allocation per row instead
/// of two, on a path that runs at 60fps with a row for every agent on the canvas.
fn ellipsise_in_place(value: &mut String, width: usize) {
    if value.chars().count() <= width {
        return;
    }
    // The byte offset of the character after the last one kept, so a multi-byte glyph is never
    // cut down the middle — `String::truncate` panics on a boundary that is not one.
    let cut = value
        .char_indices()
        .nth(width.saturating_sub(1))
        .map_or(value.len(), |(offset, _)| offset);
    value.truncate(cut);
    value.push('…');
}

fn ellipsise(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_owned();
    }
    value
        .chars()
        .take(width.saturating_sub(1))
        .chain(std::iter::once('…'))
        .collect()
}

/// Picks the richest form of the "stopped following" marker that fits, and returns the title
/// shortened to make room for it.
///
/// The marker and the title are painted as two independent titles on the same border row (see
/// the call site), and neither one knows the other is there — so without a shared reservation,
/// whichever is drawn last simply paints over the other. For the two explanatory rungs — the
/// full sentence and the row count alone — this is the reservation: try each from richest to
/// sparsest, and take the first that still leaves `MIN_TITLE_WIDTH` columns of the title
/// standing, so a pane wide enough to explain itself always keeps a recognisable title beside
/// the explanation.
///
/// The bare glyph is a different, unconditional floor: a pane that has silently stopped
/// following its own output is the exact failure this marker exists to catch, and a divider can
/// be dragged to widths well under `MIN_TITLE_WIDTH` routinely — a pane that narrow already has
/// no readable title of its own to protect. So the glyph rung asks only that the glyph and its
/// one-column separator fit inside the border; the title gets whatever is left, down to one column.
/// Only below that — not even two columns for the glyph itself — is there no marker at all, and
/// the title is left exactly as it was.
fn fit_scroll_marker(title: &str, budget: usize, offset: usize) -> (String, Option<String>) {
    /// However short a pane's own name gets, this many columns of it must stay recognisable —
    /// enough for the state glyph, the opening space, and the whole of a short label like
    /// "editor" or "agent". Guards only the two explanatory rungs: the bare glyph is the floor
    /// the marker itself is never dropped below, so it does not compete with the title for
    /// this room at all — see the bare-glyph arm below.
    const MIN_TITLE_WIDTH: usize = 8;
    const SEPARATOR: usize = 1;
    for candidate in [
        format!("▲ {offset} rows · End to follow"),
        format!("▲ {offset} rows"),
    ] {
        let marker_width = candidate.chars().count();
        if budget >= marker_width + SEPARATOR + MIN_TITLE_WIDTH {
            return (
                ellipsise(title, budget - marker_width - SEPARATOR),
                Some(candidate),
            );
        }
    }
    // The bare glyph is the floor this function never drops below for want of room to explain
    // itself: a pane that has silently stopped following its own output is the exact failure
    // this marker exists to catch, and on the narrowest panes — the ones a divider drag reaches
    // routinely, well below `MIN_TITLE_WIDTH` — the title is already unreadable on its own. So
    // this rung asks only that the glyph and its separator fit inside the border, and the title
    // takes whatever is left, even if that is only one column — `ellipsise(_, 0)` still returns
    // the "…" itself, so the title never actually disappears.
    // "▲" is one `char` (a single Unicode scalar value), so this is a literal rather than
    // `"▲".chars().count()`: the latter is not a `const fn` on stable, and the former is
    // exactly as informative for exactly one character of literal text.
    const BARE_MARKER_WIDTH: usize = 1;
    if budget >= BARE_MARKER_WIDTH + SEPARATOR {
        return (
            ellipsise(title, budget - BARE_MARKER_WIDTH - SEPARATOR),
            Some("▲".to_owned()),
        );
    }
    (title.to_owned(), None)
}

fn runtime_label(runtime: PaneRuntime) -> &'static str {
    match runtime {
        PaneRuntime::Running => "running",
        PaneRuntime::Exited => "exited",
        PaneRuntime::Restored => "restored",
        PaneRuntime::Empty => "empty",
    }
}

/// Whether a key carries a composing modifier, i.e. it is somebody reaching past the current
/// mode rather than pressing one of its letters. Shift is excluded on purpose: crossterm
/// reports an uppercase `G` or `N` with Shift set, and those are real copy-mode bindings.
fn composed(key: KeyEvent) -> bool {
    key.modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
}

/// "1 pane" / "3 panes", so the picker's detail column never reads "1 panes".
fn pane_count(panes: usize) -> String {
    if panes == 1 {
        "1 pane".into()
    } else {
        format!("{panes} panes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::PaneLayout;
    use crate::{
        adapter::{AdapterCapabilities, ProcessCapabilities},
        protocol::{ProcessState, ProviderState},
    };
    use crossterm::event::KeyEventKind;
    use ratatui::{Terminal, backend::TestBackend};
    use std::collections::BTreeMap;

    fn dashboard() -> Dashboard {
        let panes = BTreeMap::from([
            (
                "a".into(),
                PaneLayout {
                    pane_id: "a".into(),
                    name: "editor".into(),
                    run_id: None,
                    runtime: PaneRuntime::Running,
                    kind: PaneKind::Terminal,
                },
            ),
            (
                "b".into(),
                PaneLayout {
                    pane_id: "b".into(),
                    name: "agent".into(),
                    run_id: None,
                    runtime: PaneRuntime::Restored,
                    kind: PaneKind::Terminal,
                },
            ),
        ]);
        Dashboard {
            layout: LayoutSnapshot {
                workspaces: vec![WorkspaceLayout {
                    workspace_id: "w".into(),
                    name: "Daily".into(),
                    focused_pane_id: "a".into(),
                    panes,
                    root: LayoutNode::Split {
                        axis: SplitAxis::Vertical,
                        ratio_milli: 500,
                        first: Box::new(LayoutNode::Pane {
                            pane_id: "a".into(),
                        }),
                        second: Box::new(LayoutNode::Pane {
                            pane_id: "b".into(),
                        }),
                    },
                }],
            },
            ..Dashboard::default()
        }
    }

    fn prefix(dashboard: &mut Dashboard) {
        assert_eq!(
            dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)),
            UiCommand::None
        );
        assert!(dashboard.prefix_pending());
    }

    /// The open selection, if any. Copy mode holds the session inside a `CopyMode` that also
    /// owns the frozen screen, so tests reach through it rather than at a bare session.
    fn copy_selection(dashboard: &Dashboard) -> Option<((u16, u16), (u16, u16))> {
        dashboard.copy.as_ref()?.session.selection()
    }

    fn command(dashboard: &mut Dashboard, code: KeyCode) -> UiCommand {
        prefix(dashboard);
        dashboard.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn render_to_string(dashboard: &mut Dashboard, width: u16, height: u16) -> String {
        rendered(&render_terminal(dashboard, width, height))
    }

    /// The whole frame as text, which is all most tests need. Style is deliberately dropped
    /// here — anything asserting on colour must read the buffer, not this string.
    fn rendered(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn render_terminal(
        dashboard: &mut Dashboard,
        width: u16,
        height: u16,
    ) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| dashboard.render(frame)).unwrap();
        terminal
    }

    /// One row of the buffer, bounded to a rect's columns. The pane title lives on the
    /// border row, so a whole-frame string cannot tell it apart from the footer.
    fn row_text(terminal: &Terminal<TestBackend>, area: Rect, row: u16) -> String {
        let buffer = terminal.backend().buffer();
        (area.x..area.right())
            .map(|column| buffer[(column, row)].symbol())
            .collect()
    }

    /// Every cell of a pane's body carrying `background`, in grid coordinates.
    fn cells_with_background(
        terminal: &Terminal<TestBackend>,
        area: Rect,
        background: ratatui::style::Color,
    ) -> Vec<(u16, u16)> {
        let buffer = terminal.backend().buffer();
        let mut found = Vec::new();
        for row in area.y + 1..area.bottom() - 1 {
            for column in area.x + 1..area.right() - 1 {
                if buffer[(column, row)].bg == background {
                    found.push((row - area.y - 1, column - area.x - 1));
                }
            }
        }
        found
    }

    /// A full-screen seed for `run_id`, sized to the inner geometry a 100x30 dashboard gives
    /// the left pane, so nothing this feeds is clipped for a reason other than pane size.
    fn attach_event(run_id: &str, bytes: &[u8]) -> Event {
        attach_event_at(run_id, bytes, 0, 1)
    }

    /// An attach frame with a chosen paging cursor and epoch, for the tests that care where a
    /// replica's own history starts and which byte stream it belongs to.
    fn attach_event_at(run_id: &str, bytes: &[u8], history_from: u64, epoch: u64) -> Event {
        let mut source = crate::terminal::VtTerminal::new(PANE_ROWS, PANE_COLS, 0);
        source.feed(bytes);
        Event::PaneAttached {
            run_id: run_id.into(),
            revision: 1,
            rows: PANE_ROWS,
            cols: PANE_COLS,
            scrollback_rows: 2000,
            history_from,
            epoch,
            screen: STANDARD.encode(source.state_bytes()),
        }
    }

    /// One delta of raw child output. Revisions must be contiguous or `apply_event` drops the
    /// screen rather than advancing it into a corrupted grid, so this counts for the caller.
    fn delta_event(run_id: &str, revision: u64, bytes: &[u8]) -> Event {
        Event::PaneDelta {
            run_id: run_id.into(),
            revision,
            bytes: STANDARD.encode(bytes),
        }
    }

    /// Inner geometry of pane "a" when the fixture dashboard is drawn at 100x30: a two-row
    /// header and a two-row footer leave a 26-row body; the 28-column sidebar leaves 72
    /// columns, whose even vertical split gives the left pane 35; borders take one cell on
    /// each side of both axes.
    const PANE_ROWS: u16 = 24;
    /// A layout with more than one workspace spends a row on the tab strip, so its panes are
    /// exactly one row shorter than the single-workspace constant above.
    const TABBED_PANE_ROWS: u16 = PANE_ROWS - 1;
    const PANE_COLS: u16 = 33;

    /// A second, single-pane workspace so switching has somewhere to go. Its pane is bound so
    /// the switch has a PTY whose geometry must be announced.
    /// A two-workspace dashboard whose visible workspace holds a running agent, so closing it
    /// has something to lose and therefore asks before it acts.
    fn two_workspace_dashboard_with_an_agent() -> Dashboard {
        let mut dashboard = two_workspace_dashboard();
        dashboard.agents.insert(
            "run_1".into(),
            (Some(AgentKind::Claude), AgentState::Working),
        );
        dashboard
    }

    fn two_workspace_dashboard() -> Dashboard {
        let mut dashboard = bound_dashboard();
        dashboard.layout.workspaces.push(WorkspaceLayout {
            workspace_id: "w2".into(),
            name: "Deploy".into(),
            focused_pane_id: "c".into(),
            panes: BTreeMap::from([(
                "c".into(),
                PaneLayout {
                    pane_id: "c".into(),
                    name: "deploy".into(),
                    run_id: Some("run_2".into()),
                    runtime: PaneRuntime::Running,
                    kind: PaneKind::Terminal,
                },
            )]),
            root: LayoutNode::Pane {
                pane_id: "c".into(),
            },
        });
        dashboard
    }

    /// A dashboard with `count` workspaces, each named `ws{n}` — short on purpose. Mirrors
    /// `benchmark_dashboard`'s construction, but that helper names workspaces with a string long
    /// enough to be ellipsised, deliberately, so its render benchmark exercises ellipsising. With
    /// names that long barely one tab fits a narrow strip at all, which makes it useless for
    /// telling scrolling apart from truncation — these names are short so several tabs fit and
    /// the strip's own scrolling, rather than the ellipsiser, is what these tests exercise.
    fn scrollable_tab_dashboard(count: usize) -> Dashboard {
        let mut dashboard = Dashboard::default();
        for index in 0..count {
            let pane_id = format!("p{index}");
            dashboard.layout.workspaces.push(WorkspaceLayout {
                workspace_id: format!("w{index}"),
                name: format!("ws{}", index + 1),
                focused_pane_id: pane_id.clone(),
                panes: BTreeMap::from([(
                    pane_id.clone(),
                    PaneLayout {
                        pane_id: pane_id.clone(),
                        name: "pane".into(),
                        run_id: None,
                        runtime: PaneRuntime::Empty,
                        kind: PaneKind::Terminal,
                    },
                )]),
                root: LayoutNode::Pane { pane_id },
            });
        }
        dashboard
    }

    fn bound_dashboard() -> Dashboard {
        let mut dashboard = dashboard();
        dashboard.layout.workspaces[0]
            .panes
            .get_mut("a")
            .unwrap()
            .run_id = Some("run_1".into());
        dashboard
    }

    fn snapshot() -> RuntimeSnapshot {
        RuntimeSnapshot {
            binding_kind: crate::protocol::BindingKind::Repository,
            repository_root: "/repo/real".into(),
            external_task_ref: "TASK-61".into(),
            run_id: "dock_real".into(),
            worktree: "/repo/real".into(),
            branch: "main".into(),
            base_sha: "abc".into(),
            workspace_id: "w".into(),
            pane_id: "a".into(),
            state: ProcessState::Running,
            pid: Some(1),
            process_group_id: Some(1),
            command: vec!["sh".into()],
            adapter: AdapterId::Fixture,
            process_capabilities: ProcessCapabilities::OWNED_RUNTIME,
            adapter_capabilities: AdapterCapabilities::NONE,
            provider_state: ProviderState::Running,
            rows: 24,
            cols: 80,
            agent: None,
            agent_state: crate::detect::AgentState::Idle,
            title: None,
            cwd: None,
            diagnostic: None,
        }
    }

    #[test]
    fn a_paste_reaches_the_pane_as_one_bracketed_payload_with_a_single_trailing_terminator() {
        let mut dashboard = bound_dashboard();
        // The pane's program turns bracketed paste on; the client reads the mode off its own
        // replica of that pane rather than assuming it.
        dashboard.apply_event(attach_event("run_1", b"\x1b[?2004h"));
        let payload = "first\nsecond\x1b[201~rm -rf /\nthird";
        let UiCommand::PaneInput(bytes) = dashboard.paste(payload.into()) else {
            panic!("a paste into a live pane must become pane input");
        };
        assert!(bytes.starts_with(b"\x1b[200~"), "{bytes:?}");
        assert!(bytes.ends_with(b"\x1b[201~"), "{bytes:?}");
        let terminators = bytes
            .windows(6)
            .filter(|window| *window == b"\x1b[201~")
            .count();
        assert_eq!(terminators, 1, "smuggled terminator survived: {bytes:?}");
        // One input carrying every line, so nothing executes as it lands.
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("first") && text.contains("third"), "{text:?}");
    }

    #[test]
    fn a_paste_into_a_pane_that_never_enabled_the_mode_is_not_wrapped() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event("run_1", b"plain"));
        assert_eq!(
            dashboard.paste("hi".into()),
            UiCommand::PaneInput(b"hi".to_vec())
        );
    }

    #[test]
    fn an_exited_pane_is_visibly_exited_and_recoverable_from_the_keyboard() {
        let mut dashboard = bound_dashboard();
        dashboard.layout.workspaces[0]
            .panes
            .get_mut("a")
            .unwrap()
            .runtime = PaneRuntime::Exited;
        let text = render_to_string(&mut dashboard, 110, 28);
        assert!(text.contains("exited"), "{text:?}");
        assert!(text.contains("Ctrl+B R restarts"), "{text:?}");
        // Typing must not vanish into a pane with no process behind it.
        assert_eq!(
            dashboard.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            UiCommand::None
        );
        assert!(
            dashboard
                .error
                .as_deref()
                .is_some_and(|error| error.contains("Ctrl+B R")),
            "{:?}",
            dashboard.error
        );
        assert_eq!(dashboard.paste("still dropped".into()), UiCommand::None);
        assert!(matches!(
            command(&mut dashboard, KeyCode::Char('R')),
            UiCommand::Request(request)
                if matches!(request.as_ref(), Request::Workspace(WorkspaceRequest::Respawn { workspace_id, pane_id })
                    if workspace_id == "w" && pane_id == "a")
        ));
    }

    #[test]
    fn a_task_an_agent_could_not_be_launched_with_is_typed_to_it_once_it_is_up() {
        // Amp and Copilot have no prompt positional, so a dispatched card used to reach them as
        // an empty pane and the task lived only in the head of whoever pressed the key.
        let mut dashboard = bound_dashboard();
        dashboard.expect_opening_prompt("run_1", "w", "a", "fix the retry path");
        // Nothing is sent while the agent is still coming up.
        dashboard.apply_event(Event::AgentStateChanged {
            run_id: "run_1".into(),
            agent: Some(AgentKind::Amp),
            state: AgentState::Working,
        });
        assert!(dashboard.take_opening_prompts().is_empty());
        // Its first `Done` is the agent saying its input box is up and waiting.
        dashboard.apply_event(Event::AgentStateChanged {
            run_id: "run_1".into(),
            agent: Some(AgentKind::Amp),
            state: AgentState::Done,
        });
        assert_eq!(
            dashboard.take_opening_prompts(),
            vec![("w".into(), "a".into(), "fix the retry path".into())]
        );
        // Every later turn also ends in `Done`, and none of them may resend the task.
        for _ in 0..3 {
            dashboard.apply_event(Event::AgentStateChanged {
                run_id: "run_1".into(),
                agent: Some(AgentKind::Amp),
                state: AgentState::Done,
            });
        }
        assert!(dashboard.take_opening_prompts().is_empty());
    }

    #[test]
    fn a_task_waiting_on_an_agent_that_died_is_dropped_rather_than_left_pending() {
        // Otherwise it waits for a `Done` that cannot arrive, and is then delivered to whatever
        // run next inherits the id.
        let mut dashboard = bound_dashboard();
        dashboard.expect_opening_prompt("run_1", "w", "a", "fix the retry path");
        dashboard.set_runs(vec![]);
        dashboard.apply_event(Event::AgentStateChanged {
            run_id: "run_1".into(),
            agent: Some(AgentKind::Amp),
            state: AgentState::Done,
        });
        assert!(dashboard.take_opening_prompts().is_empty());
    }

    #[test]
    fn the_agent_roster_drops_entries_whose_run_is_gone() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(Event::AgentStateChanged {
            run_id: "run_1".into(),
            agent: Some(AgentKind::Claude),
            state: AgentState::Blocked,
        });
        dashboard.apply_event(Event::AgentStateChanged {
            run_id: "dock_sh_w_a".into(),
            agent: None,
            state: AgentState::Idle,
        });
        assert_eq!(dashboard.agents.len(), 2);
        let mut live = snapshot();
        live.run_id = "run_1".into();
        dashboard.set_runs(vec![live]);
        // The retired shell is gone from the roster; nothing else removed it before.
        assert_eq!(dashboard.agents.len(), 1);
        assert!(dashboard.agents.contains_key("run_1"));
        // A re-established event stream re-attaches every live run from scratch, so the roster
        // must be dropped with the screens rather than painting rows for runs that never return.
        dashboard.detach_screens();
        assert!(dashboard.agents.is_empty());
    }

    #[test]
    fn renders_split_focus_states_and_narrow_fallback() {
        for (width, height) in [(90, 24), (40, 10)] {
            let mut dashboard = dashboard();
            let text = render_to_string(&mut dashboard, width, height);
            assert!(text.contains("Daily"));
            assert!(text.contains(if width < 52 { "compact" } else { "editor" }));
        }
        // Focus is now carried entirely by the theme's border tokens rather than by a
        // per-runtime body colour, so that is what the render has to prove.
        let mut dashboard = dashboard();
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal.draw(|frame| dashboard.render(frame)).unwrap();
        let theme = Theme::warm();
        let focused = dashboard.pane_areas["a"];
        let unfocused = dashboard.pane_areas["b"];
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(focused.x, focused.y + 1)].fg, theme.border_focused);
        assert_eq!(buffer[(unfocused.x, unfocused.y + 1)].fg, theme.border);
        assert_ne!(theme.border_focused, theme.border);
    }

    #[test]
    fn keyboard_and_mouse_focus_and_bounded_resize() {
        let mut dashboard = dashboard();
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal.draw(|frame| dashboard.render(frame)).unwrap();
        prefix(&mut dashboard);
        let tab = dashboard.key(KeyEvent::new_with_kind(
            KeyCode::Tab,
            KeyModifiers::NONE,
            KeyEventKind::Press,
        ));
        assert!(
            matches!(tab, UiCommand::Request(request) if matches!(request.as_ref(), Request::Workspace(WorkspaceRequest::Focus { pane_id, .. }) if pane_id == "b"))
        );
        // `Ctrl+B Tab` already moved focus to "b", so a press there has nothing to tell the
        // daemon and must answer locally; the press that changes focus is the one on "a".
        let b = dashboard.pane_areas["b"];
        assert_eq!(
            dashboard.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: b.x + 1,
                row: b.y + 1,
                modifiers: KeyModifiers::NONE,
            }),
            UiCommand::None
        );
        let a = dashboard.pane_areas["a"];
        let focus = dashboard.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: a.x + 1,
            row: a.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        assert!(
            matches!(focus, UiCommand::Send(request) if matches!(request.as_ref(), Request::Workspace(WorkspaceRequest::Focus { pane_id, .. }) if pane_id == "a"))
        );
        let divider = dashboard.dividers[0].area;
        dashboard.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: divider.x,
            row: divider.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(
            dashboard.mouse(MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                column: 0,
                row: divider.y,
                modifiers: KeyModifiers::NONE,
            }),
            UiCommand::None,
            "the ratio is held until the button comes up; see              a_divider_drag_asks_the_daemon_to_resize_once_when_the_button_comes_up"
        );
        let resize = dashboard.mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 0,
            row: divider.y,
            modifiers: KeyModifiers::NONE,
        });
        assert!(
            matches!(resize, UiCommand::Request(request) if matches!(request.as_ref(), Request::Workspace(WorkspaceRequest::Resize { ratio_milli, .. }) if *ratio_milli > 0 && *ratio_milli < 500))
        );
    }

    /// A press that focuses a pane must not put a blocking daemon round trip in front of the
    /// drag it begins. `Send` is painted and posted; `Request` would be waited on, and then
    /// `refresh` would wait on three more — which is what made the first click of a selection
    /// hitch. The pane is focused locally either way, so nothing is lost by not waiting.
    #[test]
    fn focusing_a_pane_by_pointer_is_posted_rather_than_waited_on() {
        let mut dashboard = dashboard();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| dashboard.render(frame)).unwrap();
        let b = dashboard.pane_areas["b"];
        dashboard.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: b.x + 1,
            row: b.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        terminal.draw(|frame| dashboard.render(frame)).unwrap();
        let a = dashboard.pane_areas["a"];
        let focus = dashboard.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: a.x + 1,
            row: a.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        assert!(
            matches!(&focus, UiCommand::Send(request) if matches!(request.as_ref(),
                Request::Workspace(WorkspaceRequest::Focus { pane_id, .. }) if pane_id == "a")),
            "a pointer focus must be posted, not awaited: {focus:?}"
        );
        assert_eq!(
            dashboard.workspace().unwrap().focused_pane_id,
            "a",
            "and the focus must already be applied locally, or the paint would show the old pane"
        );
    }

    #[test]
    fn resize_to_narrow_during_drag_clears_stale_divider_safely() {
        let mut dashboard = dashboard();
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal.draw(|frame| dashboard.render(frame)).unwrap();
        let divider = dashboard.dividers[0].area;
        dashboard.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: divider.x,
            row: divider.y,
            modifiers: KeyModifiers::NONE,
        });
        assert!(dashboard.dragging.is_some());
        terminal.backend_mut().resize(40, 10);
        terminal.draw(|frame| dashboard.render(frame)).unwrap();
        assert!(dashboard.dragging.is_none());
        assert_eq!(
            dashboard.mouse(MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                column: 1,
                row: 1,
                modifiers: KeyModifiers::NONE,
            }),
            UiCommand::None
        );
    }

    #[test]
    fn generated_ids_skip_ids_restored_from_persisted_snapshot() {
        let mut dashboard = dashboard();
        dashboard.layout.workspaces[0].workspace_id = "workspace_1".into();
        dashboard.layout.workspaces[0].panes.insert(
            "workspace_2".into(),
            PaneLayout {
                pane_id: "workspace_2".into(),
                name: "collision".into(),
                run_id: None,
                runtime: PaneRuntime::Restored,
                kind: PaneKind::Terminal,
            },
        );
        dashboard.layout.workspaces[0].panes.insert(
            "pane_3".into(),
            PaneLayout {
                pane_id: "pane_3".into(),
                name: "persisted".into(),
                run_id: None,
                runtime: PaneRuntime::Restored,
                kind: PaneKind::Terminal,
            },
        );
        let create = command(&mut dashboard, KeyCode::Char('n'));
        assert!(matches!(
            create,
            UiCommand::Request(request)
                if matches!(request.as_ref(), Request::Workspace(WorkspaceRequest::Create { workspace_id, pane_id, .. })
                    if workspace_id == "workspace_4" && pane_id == "pane_5")
        ));
        let split = command(&mut dashboard, KeyCode::Char('h'));
        assert!(matches!(
            split,
            UiCommand::Request(request)
                if matches!(request.as_ref(), Request::Workspace(WorkspaceRequest::Split { new_pane_id, .. })
                    if new_pane_id == "pane_6")
        ));
    }

    #[test]
    fn binding_facts_move_into_the_pane_title_and_never_replace_the_screen() {
        let mut dashboard = dashboard();
        dashboard.layout.workspaces[0]
            .panes
            .get_mut("a")
            .unwrap()
            .run_id = Some("dock_real".into());
        dashboard.runs.push(snapshot());
        let text = render_to_string(&mut dashboard, 110, 28);
        // The body is the emulated screen now, so the run's identity has to survive in the
        // one place still reserved for facts about the binding: the pane's own title. The task
        // leads, because a run id identifies a row in a receipt and a task identifies the work —
        // and only one of those is what someone glancing at a pane is looking for.
        assert!(text.contains("#TASK-61 · dock_real"), "{text:?}");
        assert!(text.contains("agent · unbound"), "{text:?}");
        for gone in [
            "repository: /repo/real",
            "task: TASK-61",
            "binding: w/a",
            "No Dock-owned run bound",
        ] {
            assert!(!text.contains(gone), "pane body still prints {gone}");
        }
    }

    #[test]
    fn the_sidebar_menu_and_launch_row_both_work_by_key_and_by_click() {
        let mut dashboard = dashboard();
        dashboard.repository_root = "/repo".into();
        dashboard.runtime_directory = "/tmp".into();
        // `d` is a pane keystroke, and must stay one.
        assert!(!matches!(
            dashboard.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)),
            UiCommand::Request(_)
        ));
        assert_eq!(
            command(&mut dashboard, KeyCode::Char('l')),
            UiCommand::LoadCatalog
        );
        assert_eq!(
            dashboard.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            UiCommand::None
        );
        assert!(
            matches!(dashboard.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), UiCommand::Request(request)
            if matches!(request.as_ref(), Request::TerminalLaunch(request) if request.profile == DashboardProfile::Fixture && request.runtime_directory == "/tmp" && request.workspace_id == "w" && request.pane_id == "a"))
        );

        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal.draw(|frame| dashboard.render(frame)).unwrap();
        // Every menu row runs the same command its key does, so the menu is a way in rather than
        // a second vocabulary to learn.
        let board = dashboard
            .quick_action_areas
            .iter()
            .find(|(command, _)| *command == PaneCommand::Board)
            .map(|(_, area)| *area)
            .expect("the board row is on the menu");
        assert_eq!(
            dashboard.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: board.x + 1,
                row: board.y,
                modifiers: KeyModifiers::NONE
            }),
            UiCommand::LoadBoard
        );
        terminal.draw(|frame| dashboard.render(frame)).unwrap();
        let launch = dashboard.launch_area.unwrap();
        assert_eq!(
            dashboard.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: launch.x + 1,
                row: launch.y,
                modifiers: KeyModifiers::NONE
            }),
            UiCommand::LoadCatalog
        );
    }

    #[test]
    fn mouse_launch_form_selects_reviews_and_confirms_the_exact_focused_pane() {
        let mut dashboard = dashboard();
        dashboard.runtime_directory = "/tmp".into();
        dashboard.open_launch();
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal.draw(|frame| dashboard.render(frame)).unwrap();
        let profile = dashboard.launch_profile_areas[0];
        assert_eq!(
            dashboard.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: profile.x,
                row: profile.y,
                modifiers: KeyModifiers::NONE
            }),
            UiCommand::None
        );
        let confirm = dashboard.launch_confirm_area.unwrap();
        assert_eq!(
            dashboard.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: confirm.x,
                row: confirm.y,
                modifiers: KeyModifiers::NONE
            }),
            UiCommand::None
        );
        assert!(
            matches!(dashboard.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: confirm.x, row: confirm.y, modifiers: KeyModifiers::NONE }), UiCommand::Request(request)
            if matches!(request.as_ref(), Request::TerminalLaunch(request) if request.workspace_id == "w" && request.pane_id == "a"))
        );
    }

    #[test]
    fn repository_mode_constructs_only_the_existing_verified_option() {
        let mut dashboard = dashboard();
        dashboard.repository_root = "/repo".into();
        dashboard.repository_launches.push(RepositoryLaunchOption {
            task_ref: "TASK-12".into(),
            worktree: "/repo/wt".into(),
        });
        dashboard.open_launch();
        dashboard.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        dashboard.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            matches!(dashboard.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), UiCommand::Request(request)
            if matches!(request.as_ref(), Request::LaunchIntoPane(request) if request.workspace_id == "w" && request.pane_id == "a" && request.dispatch.external_task_ref == "TASK-12" && request.dispatch.worktree == "/repo/wt"))
        );
        assert!(
            PROFILES
                .iter()
                .all(|(profile, _)| AdapterId::from(*profile) != AdapterId::Generic)
        );
    }

    #[test]
    fn published_keymap_help_is_contextual_and_escape_is_local() {
        let mut dashboard = dashboard();
        assert_eq!(command(&mut dashboard, KeyCode::Char('?')), UiCommand::None);
        assert!(dashboard.help_open);
        let text = render_to_string(&mut dashboard, 90, 24);
        for key in [
            "Every key goes to the focused pane",
            "n new workspace",
            "h/v split",
            "z zoom",
            "r rename",
            "x close",
            "l launch",
            "q quit",
            "runs keep running",
            "PageUp/PageDown scroll history",
        ] {
            assert!(text.contains(key), "missing published mnemonic: {key}");
        }
        // Esc closes the overlay instead of reaching the pane, which is the one place the
        // dashboard is still allowed to keep a key for itself.
        assert_eq!(
            dashboard.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            UiCommand::None
        );
        assert!(!dashboard.help_open);
    }

    #[test]
    fn every_pane_key_is_fire_and_forget_input_and_never_a_daemon_request() {
        let mut dashboard = bound_dashboard();
        dashboard.runs.push(snapshot());
        // There is no mode to enter any more: the very first keystroke is pane input, and it
        // must be `PaneInput`, because the `Request` arm costs two daemon round trips before
        // the echo can be painted.
        for (key, expected) in [
            (
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
                b"q".to_vec(),
            ),
            (KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), vec![0x1b]),
            (
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                b"\r".to_vec(),
            ),
            (
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                vec![0x03],
            ),
        ] {
            let outcome = dashboard.key(key);
            assert_eq!(
                outcome,
                UiCommand::PaneInput(expected),
                "{key:?} must be fire-and-forget pane input"
            );
            assert!(
                !matches!(outcome, UiCommand::Request(_)),
                "{key:?} took the slow request path"
            );
        }
        // Only the prefixed form still commands the dashboard.
        assert_eq!(command(&mut dashboard, KeyCode::Char('q')), UiCommand::Quit);
        assert_eq!(command(&mut dashboard, KeyCode::Char('d')), UiCommand::Quit);
    }

    #[test]
    fn launch_typeahead_review_and_safe_choice_retention_are_pointer_independent() {
        let mut dashboard = dashboard();
        dashboard.runtime_directory = "/tmp".into();
        dashboard.open_launch();
        for character in "fix".chars() {
            assert_eq!(
                dashboard.key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)),
                UiCommand::None
            );
        }
        assert_eq!(dashboard.launch_form.as_ref().unwrap().index, 0);
        assert_eq!(
            dashboard.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            UiCommand::None
        );
        assert!(dashboard.launch_form.as_ref().unwrap().confirming);
        assert!(
            matches!(dashboard.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), UiCommand::Request(request) if matches!(request.as_ref(), Request::TerminalLaunch(request) if request.profile == DashboardProfile::Fixture))
        );
        dashboard.open_launch();
        let retained = dashboard.launch_form.as_ref().unwrap();
        assert_eq!(retained.index, 0);
        assert!(!retained.repository_mode);
        assert!(retained.query.is_empty());
    }

    #[test]
    fn focus_split_resize_and_forms_change_locally_before_requests_complete() {
        let mut dashboard = dashboard();
        let focus = command(&mut dashboard, KeyCode::Tab);
        assert!(matches!(focus, UiCommand::Request(_)));
        assert_eq!(dashboard.workspace().unwrap().focused_pane_id, "b");
        prefix(&mut dashboard);
        let back = dashboard.key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert!(matches!(back, UiCommand::Request(_)));
        assert_eq!(dashboard.workspace().unwrap().focused_pane_id, "a");
        let focus = command(&mut dashboard, KeyCode::Tab);
        assert!(matches!(focus, UiCommand::Request(_)));
        let panes = dashboard.workspace().unwrap().panes.len();
        let split = command(&mut dashboard, KeyCode::Char('h'));
        assert!(matches!(split, UiCommand::Request(_)));
        assert_eq!(dashboard.workspace().unwrap().panes.len(), panes + 1);
        assert_eq!(command(&mut dashboard, KeyCode::Char('r')), UiCommand::None);
        assert!(dashboard.rename_form.is_some());
        assert_eq!(
            dashboard.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            UiCommand::None
        );
        assert!(dashboard.rename_form.is_none());
        assert_eq!(
            command(&mut dashboard, KeyCode::Char('l')),
            UiCommand::LoadCatalog
        );
        assert!(dashboard.launch_form.is_some());
        assert_eq!(
            dashboard.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            UiCommand::None
        );
        assert!(dashboard.launch_form.is_none());
    }

    #[test]
    fn unavailable_actions_always_explain_the_reason() {
        let mut dashboard = Dashboard::default();
        for key in ['h', 'r', 'x', '+', 'z'] {
            assert_eq!(command(&mut dashboard, KeyCode::Char(key)), UiCommand::None);
            assert!(
                dashboard
                    .error
                    .as_deref()
                    .is_some_and(|message| message.contains("unavailable")),
                "{key} silently no-op'd"
            );
        }
    }

    #[test]
    fn a_bound_pane_renders_emulated_screen_content_not_binding_facts() {
        let mut dashboard = bound_dashboard();
        // Twenty distinct rows, not one line: a screen with a couple of lines on it is
        // rendered identically whether the pane is 24 rows tall or 4, so only output that
        // fills the pane can prove the geometry as well as the content.
        let mut output = Vec::new();
        for index in 1..=20 {
            output.extend_from_slice(format!("screen row {index:02}\r\n").as_bytes());
        }
        dashboard.apply_event(attach_event("run_1", &output));
        let rendered = render_to_string(&mut dashboard, 100, 30);
        for index in 1..=20 {
            assert!(
                rendered.contains(&format!("screen row {index:02}")),
                "row {index} missing, so the pane is not being drawn at its full height"
            );
        }
        assert!(!rendered.contains("No Dock-owned run bound"));
    }

    #[test]
    fn an_unattached_pane_shows_a_placeholder_until_its_first_screen_arrives() {
        let mut dashboard = bound_dashboard();
        assert!(render_to_string(&mut dashboard, 100, 30).contains("starting…"));
        dashboard.apply_event(attach_event("run_1", b"shell is up\r\n"));
        let rendered = render_to_string(&mut dashboard, 100, 30);
        assert!(rendered.contains("shell is up"));
    }

    #[test]
    fn keys_reach_the_pane_and_the_prefix_opens_command_mode() {
        let mut dashboard = bound_dashboard();
        let outcome = dashboard.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(matches!(outcome, UiCommand::PaneInput(bytes) if bytes == b"x"));
        let pending = dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        assert_eq!(pending, UiCommand::None);
        assert!(dashboard.prefix_pending());
        // A second prefix is the escape hatch for a literal Ctrl+B in the pane.
        assert!(matches!(
            dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)),
            UiCommand::PaneInput(bytes) if bytes == vec![0x02]
        ));
        assert!(!dashboard.prefix_pending());
    }

    #[test]
    fn which_key_hints_appear_only_while_the_prefix_is_pending() {
        let mut dashboard = dashboard();
        let quiet = render_to_string(&mut dashboard, 100, 30);
        assert!(!quiet.contains("split"), "{quiet:?}");
        assert!(quiet.contains("Ctrl+B ? help"), "{quiet:?}");
        prefix(&mut dashboard);
        let pending = render_to_string(&mut dashboard, 100, 30);
        // "quit" is the last entry in the table, so asserting it proves the whole bar fits
        // inside the two-row footer rather than being silently clipped.
        for hint in [
            "split",
            "zoom",
            "runs keep running",
            "focus",
            "workspace",
            // The newest hint, and therefore the one nearest the point where the bar stops
            // fitting the two-row footer.
            "copy mode",
            "quit",
        ] {
            assert!(pending.contains(hint), "missing which-key hint {hint}");
        }
    }

    #[test]
    fn a_long_sidebar_label_is_shortened_rather_than_wrapped_onto_a_second_line() {
        let mut dashboard = dashboard();
        // Workspace names are the one piece of user-supplied text in the sidebar, so they are
        // what can outrun the 27 columns the right border leaves.
        dashboard.layout.workspaces[0].name = "release train for the whole fleet".into();
        let rows = sidebar_rows(&mut dashboard, 100, 30);
        assert!(
            rows.iter()
                .any(|row| row.contains("release train for the wh…")),
            "{rows:#?}"
        );
        assert!(
            !rows
                .iter()
                .any(|row| row.contains("release train for the whole fleet")),
            "{rows:#?}"
        );
    }

    #[test]
    fn sidebar_lists_agents_with_blocked_first() {
        let mut dashboard = dashboard();
        dashboard.apply_event(Event::AgentStateChanged {
            run_id: "run_idle".into(),
            agent: Some(AgentKind::Amp),
            state: AgentState::Idle,
        });
        dashboard.apply_event(Event::AgentStateChanged {
            run_id: "run_blocked".into(),
            agent: Some(AgentKind::Claude),
            state: AgentState::Blocked,
        });
        let rendered = render_to_string(&mut dashboard, 100, 30);
        let claude = rendered.find("claude").expect("claude listed");
        let amp = rendered.find("amp").expect("amp listed");
        assert!(claude < amp, "blocked agents must sort above idle ones");
    }

    #[test]
    fn a_bound_pane_is_resized_to_its_inner_geometry_and_only_when_it_changes() {
        let mut dashboard = bound_dashboard();
        render_to_string(&mut dashboard, 100, 30);
        let first = dashboard.take_pending_resizes();
        assert_eq!(
            first,
            vec![("w".to_owned(), "a".to_owned(), PANE_ROWS, PANE_COLS)],
            "only the pane with a run needs a PTY, and it must be told its exact inner size"
        );
        render_to_string(&mut dashboard, 100, 30);
        assert!(
            dashboard.take_pending_resizes().is_empty(),
            "unchanged geometry must not re-send"
        );
        render_to_string(&mut dashboard, 120, 40);
        assert_eq!(dashboard.take_pending_resizes().len(), 1);
    }

    #[test]
    fn zoom_gives_the_focused_pane_the_whole_body_and_resizes_its_pty() {
        let mut dashboard = bound_dashboard();
        render_to_string(&mut dashboard, 100, 30);
        dashboard.take_pending_resizes();
        assert_eq!(command(&mut dashboard, KeyCode::Char('z')), UiCommand::None);
        render_to_string(&mut dashboard, 100, 30);
        let zoomed = dashboard.take_pending_resizes();
        // The body is 72 columns wide; zoomed, pane "a" owns all of it rather than the 35
        // its half of the vertical split gave it.
        assert_eq!(
            zoomed,
            vec![("w".to_owned(), "a".to_owned(), PANE_ROWS, 70)]
        );
        assert!(
            !dashboard.pane_areas.contains_key("b"),
            "b is hidden while a is zoomed"
        );
        assert_eq!(command(&mut dashboard, KeyCode::Char('z')), UiCommand::None);
        render_to_string(&mut dashboard, 100, 30);
        assert_eq!(
            dashboard.take_pending_resizes(),
            vec![("w".to_owned(), "a".to_owned(), PANE_ROWS, PANE_COLS)]
        );
    }

    #[test]
    fn workspace_cycling_moves_the_rendered_workspace_and_saturates_at_both_ends() {
        let mut dashboard = two_workspace_dashboard();
        let first = render_to_string(&mut dashboard, 100, 30);
        assert!(first.contains("editor"));
        assert!(
            !first.contains("deploy"),
            "only the selected workspace is rendered"
        );
        assert_eq!(command(&mut dashboard, KeyCode::Char('.')), UiCommand::None);
        assert_eq!(dashboard.workspace_index, 1);
        let second = render_to_string(&mut dashboard, 100, 30);
        assert!(second.contains("Deploy"), "{second:?}");
        assert!(
            second.contains("deploy · run_2"),
            "the second workspace is not rendered"
        );
        assert!(
            !second.contains("editor"),
            "the first workspace is still on screen"
        );
        // Saturating rather than wrapping: `.` at the last workspace is a mis-press, and
        // silently jumping to the first would move the user somewhere they did not ask for.
        assert_eq!(command(&mut dashboard, KeyCode::Char('.')), UiCommand::None);
        assert_eq!(dashboard.workspace_index, 1);
        assert_eq!(command(&mut dashboard, KeyCode::Char(',')), UiCommand::None);
        assert_eq!(dashboard.workspace_index, 0);
        assert_eq!(command(&mut dashboard, KeyCode::Char(',')), UiCommand::None);
        assert_eq!(dashboard.workspace_index, 0);
        assert!(render_to_string(&mut dashboard, 100, 30).contains("editor"));
    }

    #[test]
    fn the_workspace_picker_jumps_by_name_instead_of_walking_past_everything_between() {
        let mut dashboard = two_workspace_dashboard();
        render_to_string(&mut dashboard, 100, 30);
        assert_eq!(dashboard.workspace_index, 0);

        assert_eq!(command(&mut dashboard, KeyCode::Char('w')), UiCommand::None);
        let overlay = render_to_string(&mut dashboard, 100, 30);
        assert!(overlay.contains("WORKSPACES"), "{overlay:?}");
        // Both workspaces are offered, each with the digit that would reach it directly.
        assert!(overlay.contains("Daily"), "{overlay:?}");
        assert!(overlay.contains("Deploy"), "{overlay:?}");
        assert!(overlay.contains("1 pane"), "{overlay:?}");

        for character in "Dep".chars() {
            dashboard.key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        let narrowed = render_to_string(&mut dashboard, 100, 30);
        // Only the overlay's own listing is asserted on: "Daily" also names the tab behind it,
        // so its absence from the whole frame would never be true.
        let listing = narrowed
            .split("WORKSPACES ─")
            .nth(1)
            .expect("the overlay is on screen");
        assert!(
            !listing.contains("Daily"),
            "the query should have hidden the non-matching workspace: {listing:?}"
        );
        assert!(listing.contains("Deploy"), "{listing:?}");
        dashboard.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(dashboard.workspace_index, 1);
        assert!(dashboard.picker.is_none(), "taking a row closes the picker");
        assert!(render_to_string(&mut dashboard, 100, 30).contains("deploy · run_2"));
    }

    #[test]
    fn an_open_picker_swallows_every_key_so_none_reaches_the_pane() {
        let mut dashboard = two_workspace_dashboard();
        render_to_string(&mut dashboard, 100, 30);
        assert_eq!(command(&mut dashboard, KeyCode::Char('w')), UiCommand::None);
        // `x` closes a pane and `q` quits when the picker is not up. While it is, both are
        // query text and neither may reach the keymap or the PTY.
        for character in ['x', 'q'] {
            assert_eq!(
                dashboard.key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)),
                UiCommand::None
            );
        }
        assert_eq!(
            dashboard.picker.as_ref().expect("picker open").1.query(),
            "xq"
        );
        dashboard.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(dashboard.picker.is_none(), "Esc closes the picker");
        assert_eq!(dashboard.workspace_index, 0, "cancelling moves nothing");
    }

    #[test]
    fn a_capital_letter_is_query_text_rather_than_a_swallowed_modifier() {
        let mut dashboard = two_workspace_dashboard();
        render_to_string(&mut dashboard, 100, 30);
        command(&mut dashboard, KeyCode::Char('w'));
        // crossterm reports SHIFT on every capital, so excluding it would make a workspace whose
        // name starts with one unreachable by typing that name.
        dashboard.key(KeyEvent::new(KeyCode::Char('D'), KeyModifiers::SHIFT));
        assert_eq!(
            dashboard.picker.as_ref().expect("picker open").1.query(),
            "D"
        );
    }

    #[test]
    fn a_digit_jumps_to_that_tab_and_a_position_that_is_not_there_reports_instead() {
        let mut dashboard = two_workspace_dashboard();
        render_to_string(&mut dashboard, 100, 30);
        assert_eq!(command(&mut dashboard, KeyCode::Char('2')), UiCommand::None);
        assert_eq!(dashboard.workspace_index, 1);
        assert_eq!(command(&mut dashboard, KeyCode::Char('1')), UiCommand::None);
        assert_eq!(dashboard.workspace_index, 0);

        assert_eq!(command(&mut dashboard, KeyCode::Char('9')), UiCommand::None);
        assert_eq!(
            dashboard.workspace_index, 0,
            "an absent position moves nothing"
        );
        assert_eq!(dashboard.error.as_deref(), Some("no workspace 9"));
    }

    #[test]
    fn the_tab_strip_only_costs_a_row_once_there_is_a_choice_to_make() {
        let mut single = bound_dashboard();
        render_to_string(&mut single, 100, 30);
        assert!(
            single.tab_areas.is_empty(),
            "one workspace is not a choice, so the strip must not take a row"
        );

        let mut dashboard = two_workspace_dashboard();
        let frame = render_to_string(&mut dashboard, 100, 30);
        assert_eq!(dashboard.tab_areas.len(), 2);
        assert!(frame.contains("1 Daily"), "{frame:?}");
        assert!(frame.contains("2 Deploy"), "{frame:?}");
    }

    #[test]
    fn clicking_a_tab_switches_to_it() {
        let mut dashboard = two_workspace_dashboard();
        render_to_string(&mut dashboard, 100, 30);
        let (_, second) = dashboard.tab_areas[1];
        assert_eq!(
            dashboard.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: second.x,
                row: second.y,
                modifiers: KeyModifiers::NONE,
            }),
            UiCommand::None
        );
        assert_eq!(dashboard.workspace_index, 1);
    }

    #[test]
    fn the_active_tab_stays_fully_visible_however_many_workspaces_there_are() {
        let mut dashboard = scrollable_tab_dashboard(12);
        for index in 0..12 {
            dashboard.jump_to_workspace((index + 1) as u8);
            let rendered = render_to_string(&mut dashboard, 60, 24);
            let name = format!("{} ws{}", index + 1, index + 1);
            assert!(
                rendered.contains(&name),
                "workspace {} must be visible when it is active: {rendered}",
                index + 1
            );
            assert!(
                rendered.contains('✎') && rendered.contains('✘'),
                "the active tab's own affordances must not be what falls off the edge: {rendered}"
            );
        }
    }

    #[test]
    fn jumping_to_an_offscreen_workspace_scrolls_it_fully_into_view() {
        // A jump is the one thing allowed to re-anchor the strip: this dedicated test exists
        // because that correction is now conditional on a jump having happened, rather than
        // running unconditionally every frame, and a conditional correction is one that can
        // silently stop firing.
        let mut dashboard = scrollable_tab_dashboard(12);
        render_to_string(&mut dashboard, 60, 24);
        assert_eq!(dashboard.jump_to_workspace(11), UiCommand::None);
        let rendered = render_to_string(&mut dashboard, 60, 24);
        assert!(
            rendered.contains("11 ws11"),
            "the jumped-to workspace must be scrolled into view: {rendered}"
        );
        assert!(
            rendered.contains('✎') && rendered.contains('✘'),
            "its own affordances must come along, not be clipped: {rendered}"
        );
    }

    #[test]
    fn the_strip_marks_the_tabs_it_is_hiding_on_each_side() {
        // The whole-frame string is the wrong thing to search here: the sidebar's own "current
        // workspace" marker in its WORKSPACES list is the same '›' glyph, present for any active
        // workspace whether or not the tab strip itself is scrolled. Reading just the tab-strip
        // row is what actually tells the two apart.
        let mut dashboard = scrollable_tab_dashboard(12);
        dashboard.jump_to_workspace(7);
        let terminal = render_terminal(&mut dashboard, 60, 24);
        let strip = row_text(&terminal, Rect::new(0, 2, 60, 1), 2);
        assert!(strip.contains('‹') && strip.contains('›'), "{strip:?}");
    }

    #[test]
    fn a_strip_that_fits_shows_no_markers() {
        // Same reasoning as above: the sidebar's own '›' for the active workspace would make a
        // whole-frame search for the glyph pass or fail for the wrong reason. Scope the read to
        // the tab-strip row alone.
        let mut dashboard = scrollable_tab_dashboard(2);
        let terminal = render_terminal(&mut dashboard, 120, 24);
        let strip = row_text(&terminal, Rect::new(0, 2, 120, 1), 2);
        assert!(!strip.contains('‹') && !strip.contains('›'), "{strip:?}");
    }

    #[test]
    fn the_wheel_over_the_strip_scrolls_it_without_switching_workspace() {
        let mut dashboard = scrollable_tab_dashboard(12);
        let active = dashboard.workspace_index;
        let before = render_to_string(&mut dashboard, 60, 24);
        dashboard.mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 10,
            row: 2, // the tab strip row
            modifiers: KeyModifiers::NONE,
        });
        let after = render_to_string(&mut dashboard, 60, 24);
        assert_ne!(before, after, "the strip must move");
        assert_eq!(
            dashboard.workspace_index, active,
            "scrolling the strip must not switch workspace"
        );
    }

    #[test]
    fn scrolling_the_strip_by_wheel_is_not_reverted_by_the_next_render() {
        // The bounds clamp runs every frame; the bring-into-view correction must not. If it did,
        // a second render after the wheel event would silently undo the scroll — exactly the
        // contradiction that made the previous test unsatisfiable until this distinction was
        // drawn between the two clamps.
        let mut dashboard = scrollable_tab_dashboard(12);
        dashboard.mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 10,
            row: 2,
            modifiers: KeyModifiers::NONE,
        });
        let once = render_to_string(&mut dashboard, 60, 24);
        let twice = render_to_string(&mut dashboard, 60, 24);
        assert_eq!(
            once, twice,
            "re-rendering with no new input must not move the strip back"
        );
        assert_eq!(dashboard.workspace_index, 0);
    }

    #[test]
    fn clicking_the_right_marker_scrolls_the_strip_without_switching_workspace() {
        let mut dashboard = scrollable_tab_dashboard(12);
        let before = render_to_string(&mut dashboard, 60, 24);
        let marker = dashboard
            .tab_scroll_right_area
            .expect("a strip this full must show a right marker");
        assert_eq!(
            dashboard.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: marker.x,
                row: marker.y,
                modifiers: KeyModifiers::NONE,
            }),
            UiCommand::None
        );
        let after = render_to_string(&mut dashboard, 60, 24);
        assert_ne!(before, after, "the strip must move");
        assert_eq!(dashboard.workspace_index, 0);
    }

    #[test]
    fn clicking_the_left_marker_scrolls_the_strip_back() {
        let mut dashboard = scrollable_tab_dashboard(12);
        dashboard.jump_to_workspace(12);
        render_to_string(&mut dashboard, 60, 24);
        let scrolled = dashboard.tab_scroll;
        assert!(
            scrolled > 0,
            "jumping to the last workspace must have scrolled the strip"
        );
        let marker = dashboard
            .tab_scroll_left_area
            .expect("a strip scrolled away from the start must show a left marker");
        assert_eq!(
            dashboard.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: marker.x,
                row: marker.y,
                modifiers: KeyModifiers::NONE,
            }),
            UiCommand::None
        );
        render_to_string(&mut dashboard, 60, 24);
        assert!(
            dashboard.tab_scroll < scrolled,
            "the left marker must scroll back toward the start"
        );
        assert_eq!(
            dashboard.workspace_index, 11,
            "clicking a marker must not switch workspace"
        );
    }

    #[test]
    fn taking_a_workspace_that_closed_while_the_picker_was_open_reports_rather_than_moving() {
        let mut dashboard = two_workspace_dashboard();
        render_to_string(&mut dashboard, 100, 30);
        command(&mut dashboard, KeyCode::Char('w'));
        for character in "Dep".chars() {
            dashboard.key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        // The daemon owns workspaces, so one can be closed from another client between the
        // picker listing it and the user taking it. Positions listed earlier are therefore not
        // trustworthy, which is why the id is looked up again on the way out.
        dashboard
            .layout
            .workspaces
            .retain(|w| w.workspace_id != "w2");
        dashboard.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(dashboard.workspace_index, 0);
        assert_eq!(dashboard.error.as_deref(), Some("that workspace is gone"));
    }

    #[test]
    fn the_file_picker_lists_the_focused_panes_directory_and_types_the_choice_into_it() {
        let tree = std::env::temp_dir().join(format!("dock-picker-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tree);
        std::fs::create_dir_all(tree.join("src")).unwrap();
        std::fs::write(tree.join("README.md"), "x").unwrap();
        std::fs::write(tree.join("src/main.rs"), "x").unwrap();

        let mut dashboard = bound_dashboard();
        // The pane reports where it is through OSC 7, so the listing follows the shell's `cd`
        // rather than staying pinned to wherever the pane was launched.
        let mut run = snapshot();
        run.run_id = "run_1".into();
        run.cwd = Some(tree.to_string_lossy().into_owned());
        dashboard.runs = vec![run];
        render_to_string(&mut dashboard, 100, 30);

        assert_eq!(command(&mut dashboard, KeyCode::Char('f')), UiCommand::None);
        let overlay = render_to_string(&mut dashboard, 100, 30);
        assert!(overlay.contains("FILES"), "{overlay:?}");
        assert!(overlay.contains("README.md"), "{overlay:?}");
        assert!(overlay.contains("main.rs"), "{overlay:?}");

        for character in "main".chars() {
            dashboard.key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        // Taking a row types the path into the pane instead of opening it: Dock cannot know
        // which verb was wanted, and the path is what every verb needs.
        assert_eq!(
            dashboard.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            UiCommand::PaneInput(b"src/main.rs ".to_vec())
        );
        assert!(dashboard.picker.is_none());
        let _ = std::fs::remove_dir_all(&tree);
    }

    #[test]
    fn a_pane_with_nowhere_to_list_says_so_rather_than_opening_an_empty_picker() {
        let mut dashboard = bound_dashboard();
        dashboard.repository_root = String::new();
        render_to_string(&mut dashboard, 100, 30);
        assert_eq!(command(&mut dashboard, KeyCode::Char('f')), UiCommand::None);
        assert!(dashboard.picker.is_none());
        assert_eq!(
            dashboard.error.as_deref(),
            Some("file picker unavailable: this pane has no directory")
        );
    }

    #[test]
    fn resuming_an_unbound_agent_relaunches_it_asking_to_continue_its_own_session() {
        let mut dashboard = bound_dashboard();
        let mut run = snapshot();
        run.run_id = "run_1".into();
        run.binding_kind = BindingKind::Terminal;
        run.adapter = AdapterId::ClaudeCode;
        run.external_task_ref = String::new();
        run.worktree = "/somewhere/notes".into();
        dashboard.runs = vec![run];
        render_to_string(&mut dashboard, 100, 30);

        let request = command(&mut dashboard, KeyCode::Char('a'));
        let UiCommand::Request(request) = request else {
            panic!("resume must issue a launch, got {request:?}");
        };
        match *request {
            Request::TerminalLaunch(launch) => {
                // Continuing is asked for by argument, so the agent finds its own transcript.
                assert_eq!(launch.arguments, vec!["--continue".to_owned()]);
                // In the directory the conversation was filed under, not the dashboard's own.
                assert_eq!(launch.runtime_directory, "/somewhere/notes");
                assert_eq!(launch.pane_id, "a");
            }
            other => panic!("an unbound run resumes as a terminal launch, got {other:?}"),
        }
    }

    #[test]
    fn resuming_a_repository_bound_agent_keeps_the_task_and_worktree_its_session_belongs_to() {
        let mut dashboard = bound_dashboard();
        let mut run = snapshot();
        run.run_id = "run_1".into();
        run.binding_kind = BindingKind::Repository;
        run.adapter = AdapterId::CodexCli;
        dashboard.runs = vec![run];
        render_to_string(&mut dashboard, 100, 30);

        let UiCommand::Request(request) = command(&mut dashboard, KeyCode::Char('a')) else {
            panic!("resume must issue a launch");
        };
        match *request {
            Request::LaunchIntoPane(launch) => {
                assert_eq!(
                    launch.dispatch.adapter.arguments,
                    vec!["resume".to_owned(), "--last".to_owned()]
                );
                assert_eq!(launch.dispatch.external_task_ref, "TASK-61");
                assert_eq!(launch.dispatch.worktree, "/repo/real");
            }
            other => panic!("a bound run resumes into its pane, got {other:?}"),
        }
    }

    #[test]
    fn an_agent_with_no_verified_recipe_says_so_rather_than_starting_a_fresh_session() {
        let mut dashboard = bound_dashboard();
        let mut run = snapshot();
        run.run_id = "run_1".into();
        run.adapter = AdapterId::GithubCopilotCli;
        dashboard.runs = vec![run];
        render_to_string(&mut dashboard, 100, 30);

        // Silently launching without the flag would look like a resume and quietly discard the
        // conversation the user meant to continue, so nothing is sent at all.
        assert_eq!(command(&mut dashboard, KeyCode::Char('a')), UiCommand::None);
        assert_eq!(
            dashboard.error.as_deref(),
            Some("GitHub Copilot CLI cannot be resumed")
        );
    }

    #[test]
    fn resuming_a_pane_nothing_has_run_in_explains_itself() {
        let mut dashboard = dashboard();
        render_to_string(&mut dashboard, 100, 30);
        assert_eq!(command(&mut dashboard, KeyCode::Char('a')), UiCommand::None);
        assert_eq!(
            dashboard.error.as_deref(),
            Some("resume unavailable: no agent has run in this pane")
        );
    }

    fn handoff(run_id: &str, task_id: &str) -> HandoffRecord {
        HandoffRecord {
            packet: crate::model::HandoffPacket {
                schema_version: 1,
                run_id: run_id.into(),
                task_id: task_id.into(),
                workspace_id: "w".into(),
                pane_id: "a".into(),
                worktree: "/repo/real".into(),
                branch: "dock/fixture".into(),
                base_sha: "abc".into(),
                summary: "Retry added; one bounded decision remains.".into(),
                question: Some("Accept V0.1 scope?".into()),
                checks: vec![crate::model::Check {
                    name: "cargo test".into(),
                    passed: true,
                }],
            },
            evidence: crate::model::HandoffEvidence {
                branch: "dock/fixture".into(),
                base_sha: "abc".into(),
                head_sha: "def".into(),
                status_entries: 2,
                changed_files: 4,
                insertions: 12,
                deletions: 3,
            },
        }
    }

    #[test]
    fn the_review_key_asks_the_daemon_for_the_queue_rather_than_guessing_at_it() {
        let mut dashboard = bound_dashboard();
        render_to_string(&mut dashboard, 100, 30);
        assert_eq!(
            command(&mut dashboard, KeyCode::Char('i')),
            UiCommand::LoadReviewInbox
        );
    }

    #[test]
    fn an_open_review_shows_the_agents_claim_beside_the_evidence_for_it() {
        let mut dashboard = bound_dashboard();
        dashboard.set_review_inbox(vec![(handoff("dock_01J9", "DOCK-7"), None)]);
        let frame = render_to_string(&mut dashboard, 100, 30);
        assert!(frame.contains("REVIEW"), "{frame:?}");
        assert!(frame.contains("DOCK-7"), "{frame:?}");
        assert!(frame.contains("one bounded decision remains"), "{frame:?}");
        assert!(frame.contains("Accept V0.1 scope?"), "{frame:?}");
        // The measured evidence sits beside the agent's own summary, so a claim and what the
        // daemon actually observed are read together rather than one standing in for the other.
        assert!(frame.contains("4 files"), "{frame:?}");
        assert!(frame.contains("+12"), "{frame:?}");
        // The invariant is stated where the decision is taken, not only in the docs.
        assert!(frame.contains("never merged"), "{frame:?}");
    }

    #[test]
    fn a_decision_carries_the_route_and_the_note_that_justifies_it() {
        let mut dashboard = bound_dashboard();
        dashboard.set_review_inbox(vec![(handoff("dock_01J9", "DOCK-7"), None)]);
        dashboard.key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        for character in "scope ok".chars() {
            dashboard.key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        let UiCommand::Request(request) =
            dashboard.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("a completed decision must be sent");
        };
        match *request {
            Request::Decide(decide) => {
                assert_eq!(decide.run_id, "dock_01J9");
                assert_eq!(decide.route, ReviewRoute::AcceptScope);
                assert_eq!(decide.note, "scope ok");
            }
            other => panic!("expected a decision, got {other:?}"),
        }
        assert!(dashboard.review.is_none(), "sending closes the queue");
    }

    #[test]
    fn a_decision_without_a_note_is_refused_before_the_daemon_ever_sees_it() {
        let mut dashboard = bound_dashboard();
        dashboard.set_review_inbox(vec![(handoff("dock_01J9", "DOCK-7"), None)]);
        dashboard.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        // ReviewDecision::new refuses an empty note, so the note is collected here rather than
        // sending something the daemon will only bounce back.
        assert_eq!(
            dashboard.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            UiCommand::None
        );
        assert!(dashboard.review.is_some(), "the queue stays open");
        assert_eq!(
            dashboard.error.as_deref(),
            Some("a decision needs a note saying why, however short")
        );
    }

    #[test]
    fn escape_abandons_the_note_before_it_abandons_the_queue() {
        let mut dashboard = bound_dashboard();
        dashboard.set_review_inbox(vec![(handoff("dock_01J9", "DOCK-7"), None)]);
        dashboard.key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        dashboard.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        dashboard.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            dashboard
                .review
                .as_ref()
                .expect("still open")
                .pending
                .is_none(),
            "the first Esc drops the half-typed note"
        );
        dashboard.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            dashboard.review.is_none(),
            "the second Esc closes the queue"
        );
    }

    #[test]
    fn an_empty_queue_says_so_rather_than_opening_an_empty_window() {
        let mut dashboard = bound_dashboard();
        dashboard.set_review_inbox(Vec::new());
        assert!(dashboard.review.is_none());
        assert_eq!(
            dashboard.error.as_deref(),
            Some("nothing has been handed back yet")
        );
    }

    /// A dashboard whose available agents are pinned, so a dispatch test asserts the choosing
    /// rather than whatever happens to be on the machine running it.
    fn dashboard_with_agents(agents: &[AdapterId]) -> Dashboard {
        let mut dashboard = bound_dashboard();
        dashboard.installed_adapters = Some(agents.to_vec());
        dashboard
    }

    fn board_task(id: u64, title: &str, status: &str) -> BoardTask {
        BoardTask {
            id,
            title: title.into(),
            status: status.into(),
            priority: "high".into(),
            file: std::path::PathBuf::from(format!("kanban/tasks/{id}.md")),
            body: format!("# Outcome\n\n{title}"),
        }
    }

    #[test]
    fn the_board_key_asks_for_the_board_rather_than_reading_it_from_the_key_handler() {
        let mut dashboard = bound_dashboard();
        render_to_string(&mut dashboard, 100, 30);
        assert_eq!(
            command(&mut dashboard, KeyCode::Char('k')),
            UiCommand::LoadBoard
        );
    }

    #[test]
    fn a_dispatch_never_lands_on_the_test_fixture_and_says_who_it_will_land_on() {
        let mut dashboard = dashboard_with_agents(&[AdapterId::Amp, AdapterId::ClaudeCode]);
        dashboard.set_board_tasks(
            vec![board_task(1, "do the thing", "backlog")],
            crate::board::tasks_dir("", "workspace_1").expect("a workspace board"),
        );
        dashboard.board.as_mut().unwrap().writable = true;
        // The fixture sits first in the profile list and last_launch_profile starts at zero, so a
        // dashboard whose launch form was never opened used to send every task to a stub that
        // prints one line and exits — the task moved to in-progress and nothing worked on it.
        assert_ne!(dashboard.dispatch_adapter(), Some(AdapterId::Fixture));
        // Amp is listed first and takes no prompt positional, so profile order alone would pick
        // it. Dispatch exists to put an agent on a specific piece of work, and one that cannot be
        // handed the task opens in the right place knowing nothing about why.
        assert_eq!(dashboard.dispatch_adapter(), Some(AdapterId::ClaudeCode));

        // And with nothing installed there is nothing to choose, which the board has to say rather
        // than dispatch into silence.
        let mut bare = dashboard_with_agents(&[]);
        assert_eq!(bare.dispatch_adapter(), None);
        bare.set_board_tasks(
            vec![board_task(1, "do the thing", "backlog")],
            crate::board::tasks_dir("", "workspace_1").expect("a workspace board"),
        );
        bare.board.as_mut().unwrap().writable = true;
        assert!(
            render_to_string(&mut bare, 130, 32).contains("no agent installed"),
            "the board must say so rather than choose silently"
        );
        // And whichever it is, the board says so rather than choosing behind the reader's back.
        let frame = render_to_string(&mut dashboard, 130, 32);
        if let Some(adapter) = dashboard.dispatch_adapter() {
            assert!(frame.contains(adapter.label()), "{frame:?}");
        } else {
            assert!(frame.contains("no agent installed"), "{frame:?}");
        }
    }

    #[test]
    fn a_card_can_be_moved_across_columns_and_the_cursor_goes_with_it() {
        // A real board directory, because moving a card writes the task file.
        let root = std::env::temp_dir().join(format!("dock-move-{}", std::process::id()));
        let dir = root.join("tasks");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&dir).unwrap();
        crate::board::create(&dir, "wire the parser").expect("seed a task");

        let mut dashboard = bound_dashboard();
        // Point the board at this directory and make it writable the way a workspace board is.
        dashboard.set_board_tasks(crate::board::load(&dir), dir.clone());
        dashboard.board.as_mut().unwrap().writable = true;
        assert_eq!(
            dashboard.board.as_ref().unwrap().view.status(),
            Some("backlog")
        );

        // `>` is the one thing a board does that a list cannot do at all.
        dashboard.key(KeyEvent::new(KeyCode::Char('>'), KeyModifiers::NONE));
        assert_eq!(crate::board::load(&dir)[0].status, "todo", "the file moved");
        let board = dashboard.board.as_ref().unwrap();
        assert_eq!(
            board.view.status(),
            Some("todo"),
            "the cursor follows the card rather than staying over a column position"
        );
        assert_eq!(board.view.selected().map(|task| task.id), Some(1));

        dashboard.key(KeyEvent::new(KeyCode::Char('<'), KeyModifiers::NONE));
        assert_eq!(crate::board::load(&dir)[0].status, "backlog");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A dashboard whose visible workspace holds one terminal pane and one board pane, with a
    /// live agent in a second workspace so the runs lane has something to show.
    #[test]
    fn a_board_with_no_cards_says_so_rather_than_painting_five_empty_columns() {
        // What real use turned up: a workspace whose board directory does not exist yet drew
        // five headings reading `· 0` over four dashes and nothing else, filling the pane. It
        // reads as broken rather than as empty, and the first question it prompts is why the
        // agent running above it is not in one of the columns.
        let mut dashboard = dashboard_with_a_board_pane();
        dashboard.set_board_pane_tasks(Vec::new(), std::path::PathBuf::from("/tmp/none"));
        // Nothing on the board and nothing running: the fixture's agent is cleared, because an
        // agent *is* something to show and the grid is what shows it — see below.
        dashboard.runs.clear();
        dashboard.agents.clear();
        let frame = render_to_string(&mut dashboard, 160, 40);

        assert!(
            frame.contains("no tasks on this board"),
            "an empty board must say it is empty: {frame:?}"
        );
        assert!(
            frame.contains("Ctrl+B k"),
            "and name the way to add one: {frame:?}"
        );
        // The columns are what made it look broken, so they must not be drawn at all.
        assert!(
            !frame.contains("BACKLOG"),
            "no column headings over an empty board: {frame:?}"
        );
    }

    #[test]
    fn a_board_with_no_cards_but_an_agent_running_draws_the_grid_rather_than_saying_it_is_empty() {
        // The other half of the same judgement, and the half the runs lane used to cover. A
        // board whose files are empty is not an empty board while an agent is working: that
        // agent is an entry in `ACTIVE`, and the "no tasks" page would be the board hiding the
        // one thing on it.
        let mut dashboard = dashboard_with_a_board_pane();
        dashboard.set_board_pane_tasks(Vec::new(), std::path::PathBuf::from("/tmp/none"));
        let terminal = render_terminal(&mut dashboard, 400, 40);
        let frame = rendered(&terminal);
        assert!(!frame.contains("no tasks on this board"), "{frame:?}");
        let active = board_column(&terminal, "ACTIVE");
        assert!(active.contains("claude"), "{active:?}");
        assert!(active.contains("needs you"), "{active:?}");
    }

    #[test]
    fn a_run_that_came_from_no_card_says_so_in_words() {
        // The lane wrote a bare em dash where a card id goes, which is correct and unreadable —
        // it was read as "something is missing" rather than "this agent was launched by hand".
        let mut dashboard = dashboard_with_a_board_pane();
        for run in &mut dashboard.runs {
            run.external_task_ref = String::new();
        }
        // Wide enough for a card to have room for its second line, which is where an entry with
        // no card says so — see `a_narrow_active_card_gives_up_a_line_rather_than_cutting_the_
        // state_word_in_half` for what a card does when it has not.
        let frame = render_to_string(&mut dashboard, 400, 40);
        assert!(
            frame.contains("no card"),
            "a hand-launched agent should say why it has no task: {frame:?}"
        );
    }

    /// Everything drawn in one board column, found by its heading rather than by counting cells.
    ///
    /// A test that hardcoded x coordinates would be asserting about how wide the fixture's split
    /// happened to make the pane, which is not what any of them are about. The next heading on
    /// the same row is this column's right edge.
    ///
    /// The topmost column with that heading wins, so a test with a board pane *and* the overlay
    /// open reads a mixture of the two where the popup overwrites the pane. Open one or the
    /// other.
    fn board_column(terminal: &Terminal<TestBackend>, heading: &str) -> String {
        let buffer = terminal.backend().buffer();
        let area = *buffer.area();
        let symbol = |x: u16, y: u16| buffer[(x, y)].symbol().to_owned();
        let wanted: Vec<String> = heading.chars().map(|c| c.to_string()).collect();
        let width = wanted.len() as u16;
        for row in 0..area.height {
            for x in 0..area.width.saturating_sub(width) {
                if (0..width).any(|offset| symbol(x + offset, row) != wanted[usize::from(offset)]) {
                    continue;
                }
                let right = (x + width..area.width)
                    .find(|probe| {
                        symbol(*probe, row)
                            .chars()
                            .next()
                            .is_some_and(|character| character.is_ascii_uppercase())
                    })
                    .unwrap_or(area.width);
                return (row..area.height)
                    .map(|y| {
                        (x..right)
                            .map(|column| symbol(column, y))
                            .collect::<String>()
                    })
                    .collect::<Vec<String>>()
                    .join("\n");
            }
        }
        String::new()
    }

    #[test]
    fn a_live_agent_is_a_card_in_the_active_column_instead_of_a_lane_above_the_grid() {
        // What the first real use of this pane asked out loud: the user's agent was running in a
        // strip above the columns, and they asked why their work "was not in the table". One
        // grid. `ACTIVE` is the `in-progress` column under the name of what it actually holds.
        let mut dashboard = dashboard_with_a_board_pane();
        let terminal = render_terminal(&mut dashboard, 400, 40);
        let frame = rendered(&terminal);
        assert!(!frame.contains("RUNS"), "the lane is gone: {frame:?}");
        assert!(
            !frame.contains("IN-PROGRESS"),
            "and the column is called what it holds: {frame:?}"
        );

        let active = board_column(&terminal, "ACTIVE");
        assert!(active.contains("#7"), "the card is in the grid: {active:?}");
        assert!(
            active.contains("wire the parser"),
            "with what it is: {active:?}"
        );
        assert!(active.contains("claude"), "and the agent on it: {active:?}");
        assert!(
            active.contains("needs you"),
            "and what that agent is doing, spelled out: {active:?}"
        );
    }

    #[test]
    fn an_agent_launched_by_hand_is_an_entry_in_active_rather_than_missing_from_the_grid() {
        // The other half of the same complaint. A run nobody dispatched from a card has no card
        // to be drawn as, and the old lane was the only place it appeared at all — so deleting
        // the lane without this would have deleted the agent.
        let mut dashboard = dashboard_with_a_board_pane();
        for run in &mut dashboard.runs {
            run.external_task_ref = String::new();
        }
        let terminal = render_terminal(&mut dashboard, 400, 40);
        let active = board_column(&terminal, "ACTIVE");
        assert!(
            active.contains("claude"),
            "the agent is in the grid: {active:?}"
        );
        assert!(
            active.contains("no card"),
            "said in words rather than punctuated with a dash: {active:?}"
        );
    }

    #[test]
    fn a_dispatched_card_whose_agent_has_gone_stays_in_active_and_says_it_is_not_running() {
        // The card a person has forgotten about, which is exactly the one worth showing. A
        // daemon restart or a closed pane takes the run away; the card is still in `in-progress`
        // on disk and nothing about the missing run may move it.
        let mut dashboard = dashboard_with_a_board_pane();
        dashboard.runs.clear();
        dashboard.agents.clear();
        let terminal = render_terminal(&mut dashboard, 400, 40);
        let active = board_column(&terminal, "ACTIVE");
        assert!(active.contains("#7"), "the card stays: {active:?}");
        assert!(
            active.contains("not running"),
            "and says what became of its agent: {active:?}"
        );
        assert_eq!(
            dashboard.board_tasks[0].status, "in-progress",
            "and the card is where its file says it is"
        );
    }

    #[test]
    fn active_puts_the_agent_that_needs_you_above_the_one_that_is_working() {
        // The same order the sidebar roster sorts by, for the same reason: a blocked agent is
        // the only one costing the user throughput while it waits.
        let mut dashboard = dashboard_with_a_board_pane();
        dashboard.set_board_pane_tasks(
            vec![
                board_task(7, "wire the parser", "in-progress"),
                board_task(9, "write the docs", "in-progress"),
            ],
            "/repo/real/kanban/tasks".into(),
        );
        let mut second = snapshot();
        second.run_id = "run_2".into();
        second.workspace_id = "w".into();
        second.pane_id = "d".into();
        second.external_task_ref = "9".into();
        dashboard.runs.push(second);
        dashboard.agents.insert(
            "run_2".into(),
            (Some(AgentKind::Claude), AgentState::Working),
        );

        let terminal = render_terminal(&mut dashboard, 400, 40);
        let active = board_column(&terminal, "ACTIVE");
        let blocked = active.find("#7").expect("the blocked card is drawn");
        let working = active.find("#9").expect("the working card is drawn");
        assert!(
            blocked < working,
            "needs you sorts above working: {active:?}"
        );

        // And the order follows the agents rather than the file order: swap the states and the
        // column swaps with them, on the next frame and with no keypress.
        dashboard.apply_event(Event::AgentStateChanged {
            run_id: "run_1".into(),
            agent: Some(AgentKind::Claude),
            state: AgentState::Working,
        });
        dashboard.apply_event(Event::AgentStateChanged {
            run_id: "run_2".into(),
            agent: Some(AgentKind::Claude),
            state: AgentState::Blocked,
        });
        let terminal = render_terminal(&mut dashboard, 400, 40);
        let active = board_column(&terminal, "ACTIVE");
        assert!(
            active.find("#9") < active.find("#7"),
            "the column re-sorted itself: {active:?}"
        );
    }

    #[test]
    fn an_agent_bound_to_a_card_is_never_also_drawn_as_an_agent_with_no_card() {
        // The one duplication this join can produce. A dispatched card is one entry carrying its
        // run, not a card and a loose agent that happen to be the same work.
        let mut dashboard = dashboard_with_a_board_pane();
        let terminal = render_terminal(&mut dashboard, 400, 40);
        let active = board_column(&terminal, "ACTIVE");
        assert_eq!(
            active.matches("claude").count(),
            1,
            "one entry for one agent: {active:?}"
        );
        assert!(
            !active.contains("no card"),
            "this agent has a card, and it is the entry it is drawn on: {active:?}"
        );
        assert!(
            active.contains("ACTIVE \u{b7} 1"),
            "and the column counts what it draws: {active:?}"
        );
    }

    #[test]
    fn nothing_about_a_live_agent_ever_rewrites_a_status_line() {
        // The rule the whole redesign rests on. `ACTIVE` membership is derived on every frame and
        // written nowhere: the status detector calls a 1.8-second pause "finished", and a derived
        // column shows a wrong card for one frame and corrects itself where a written one leaves
        // a wrong file on disk for somebody to find tomorrow.
        let root = std::env::temp_dir().join(format!("dock-derived-{}", std::process::id()));
        let dir = root.join("tasks");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&dir).unwrap();
        crate::board::create(&dir, "wire the parser").expect("seed a task");
        let id = crate::board::load(&dir)[0].id;
        crate::board::set_status(&dir, id, "in-progress").expect("dispatch puts it in progress");
        let file = crate::board::load(&dir)[0].file.clone();
        let before = std::fs::read_to_string(&file).unwrap();
        assert!(before.contains("status: in-progress"), "{before:?}");

        let mut dashboard = dashboard_with_a_board_pane();
        dashboard.runs[0].external_task_ref = id.to_string();
        dashboard.set_board_pane_tasks(crate::board::load(&dir), dir.clone());
        for state in [
            AgentState::Blocked,
            AgentState::Working,
            AgentState::Done,
            AgentState::Idle,
        ] {
            dashboard
                .agents
                .insert("run_1".into(), (Some(AgentKind::Claude), state));
            render_to_string(&mut dashboard, 400, 40);
        }
        // And the frame that would be most tempted to move the card back: its agent is gone.
        dashboard.agents.clear();
        dashboard.runs.clear();
        render_to_string(&mut dashboard, 400, 40);

        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            before,
            "the board is files on disk, and nothing about a live agent may write one"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_narrow_active_card_gives_up_a_line_rather_than_cutting_the_state_word_in_half() {
        // The second line is what a two-line card is *for*, so it degrades in deliberate steps
        // rather than being left to the ellipsis. Five columns across 105 cells, less the
        // sidebar, leave fourteen: too narrow for `claude · a · needs you · 0 queued`, wide
        // enough for the word that matters.
        let mut dashboard = dashboard_with_a_board_pane();
        dashboard.layout.workspaces[0].root = LayoutNode::Pane {
            pane_id: "b".into(),
        };
        let terminal = render_terminal(&mut dashboard, 105, 24);
        let active = board_column(&terminal, "ACTIVE");
        assert!(
            active.contains("needs you"),
            "the state word survives whole: {active:?}"
        );
        assert!(
            !active.contains("claude"),
            "and everything that did not fit was dropped rather than cut: {active:?}"
        );

        // At 80 the same five columns leave nine cells, where even the word would be cut. The
        // card gives up its second line entirely: `nee…` is worse than nothing, and the glyph
        // and its colour are there to carry the state on their own.
        let terminal = render_terminal(&mut dashboard, 80, 24);
        let active = board_column(&terminal, "ACTIVE");
        assert!(active.contains("#7"), "the card is still drawn: {active:?}");
        assert!(
            !active.contains("\u{2026}\n") || !active.contains("needs"),
            "no half a state word anywhere: {active:?}"
        );
        assert!(
            !active.contains("needs"),
            "the second line is gone, not truncated: {active:?}"
        );
    }

    fn dashboard_with_a_board_pane() -> Dashboard {
        let mut dashboard = bound_dashboard();
        let workspace = &mut dashboard.layout.workspaces[0];
        workspace.panes.get_mut("b").unwrap().kind = PaneKind::Board;
        workspace.panes.get_mut("b").unwrap().name = "board".into();
        workspace.panes.get_mut("b").unwrap().runtime = PaneRuntime::Empty;

        // One agent pane and one plain shell, so "one row per live agent and none for a shell"
        // has both halves to distinguish.
        let mut agent = snapshot();
        agent.run_id = "run_1".into();
        agent.workspace_id = "w".into();
        agent.pane_id = "a".into();
        agent.external_task_ref = "7".into();
        dashboard.runs.push(agent);
        let mut shell = snapshot();
        shell.run_id = "dock_sh_w_c".into();
        shell.workspace_id = "w".into();
        shell.pane_id = "c".into();
        shell.external_task_ref = String::new();
        dashboard.runs.push(shell);
        dashboard.agents.insert(
            "run_1".into(),
            (Some(AgentKind::Claude), AgentState::Blocked),
        );
        // A shell has no agent kind, which is precisely what `AgentState::Idle` means here: not
        // "the agent is idle" but "no agent was detected in this pane".
        dashboard
            .agents
            .insert("dock_sh_w_c".into(), (None, AgentState::Idle));
        dashboard.set_board_pane_tasks(
            vec![
                board_task(7, "wire the parser", "in-progress"),
                board_task(9, "write the docs", "backlog"),
            ],
            "/repo/real/kanban/tasks".into(),
        );
        dashboard
    }

    #[test]
    fn a_board_pane_draws_the_whole_grid_and_never_a_terminal_placeholder() {
        // The Board pane is the whole point of giving panes a kind: it occupies a rectangle on
        // the canvas like any other pane and draws something that is not a terminal. Before this
        // it drew the "Ctrl+B R starts a shell" placeholder, which is an offer a board must never
        // make — it has no run and is never going to get one.
        let mut dashboard = dashboard_with_a_board_pane();
        let frame = render_to_string(&mut dashboard, 160, 40);

        assert!(
            frame.contains("ACTIVE"),
            "the live column is drawn: {frame:?}"
        );
        assert!(
            frame.contains("BACKLOG"),
            "and the rest of the one grid beside it: {frame:?}"
        );
        assert!(frame.contains("#9"), "with the cards in it: {frame:?}");
        assert!(
            !frame.contains("Ctrl+B R starts a shell"),
            "a board must never offer to become a terminal: {frame:?}"
        );
    }

    #[test]
    fn the_runs_lane_shows_one_row_per_live_agent_and_none_for_a_shell() {
        // Derived, never stored: one row per live agent pane, assembled from the run list and the
        // agent roster on each frame. A plain shell is a pane with no agent in it — `Idle` means
        // "nothing was detected here", not "the agent is resting" — and a lane that listed those
        // would list every pane on the canvas.
        let mut dashboard = dashboard_with_a_board_pane();
        let rows = dashboard.live_runs();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].run_id, "run_1");
        assert_eq!(rows[0].state, AgentState::Blocked);
        assert_eq!(rows[0].task_id, Some(7));
        assert_eq!(
            rows[0].queued, 0,
            "the daemon reported no queue for this pane"
        );
        assert!(!rows[0].auto_feed);

        let frame = render_to_string(&mut dashboard, 160, 40);
        assert!(
            frame.contains("needs you"),
            "the lane says what the state means, because a coloured glyph says only that \
             something changed: {frame:?}"
        );
        assert!(!frame.contains("dock_sh_w_c"), "{frame:?}");
    }

    #[test]
    fn a_backlog_card_bound_to_a_live_run_is_badged_with_that_runs_state() {
        // The one join between the lanes, and it is display-only. Dock measures; the agent
        // reports. A badge is derived and vanishes with the run that produced it; a status write
        // is durable and outlives whatever misread produced it, which is why nothing here moves
        // the card to `needs-input` however blocked its agent is.
        let mut dashboard = dashboard_with_a_board_pane();
        let terminal = render_terminal(&mut dashboard, 160, 40);
        let blocked = dashboard.theme.agent(AgentState::Blocked);
        let buffer = terminal.backend().buffer();

        let badged = buffer
            .content
            .iter()
            .enumerate()
            .filter(|(_, cell)| cell.symbol() == "●" && cell.fg == blocked)
            .count();
        assert!(
            badged >= 2,
            "the blocked run is badged in the runs lane and again on the card it is bound to"
        );
        // And the card file is untouched: the board is the durable record of what happened.
        assert_eq!(dashboard.board_tasks[0].status, "in-progress");
    }

    #[test]
    fn an_agent_state_change_reaches_the_board_pane_on_the_next_frame_with_no_keypress() {
        // Requirement 3, and it costs nothing on the wire. `Event::AgentStateChanged` already
        // lands in `self.agents`, and the runs lane is assembled from `self.agents` per frame —
        // so the frame after the event already says the new thing. Deliberately asserted without
        // a keypress and without `needs_refresh`: a state transition does not invalidate the run
        // list, and marking it dirty would put a daemon round trip behind every flicker of a busy
        // agent's classifier.
        let mut dashboard = dashboard_with_a_board_pane();
        assert!(render_to_string(&mut dashboard, 160, 40).contains("needs you"));

        dashboard.apply_event(Event::AgentStateChanged {
            run_id: "run_1".into(),
            agent: Some(AgentKind::Claude),
            state: AgentState::Working,
        });
        assert!(
            !dashboard.needs_refresh,
            "a state transition must not ask the daemon for the run list again"
        );
        let frame = render_to_string(&mut dashboard, 160, 40);
        assert!(frame.contains("working"), "{frame:?}");
        assert!(!frame.contains("needs you"), "{frame:?}");
    }

    /// Puts the board's cursor on the first entry in `ACTIVE`.
    ///
    /// The one cursor opens where the view opens — the leftmost column holding anything, which
    /// in these fixtures is the backlog — so every claim about arming starts by moving into the
    /// column arming happens in. This is what deleting the lane changed about these tests: the
    /// target is the grid cursor now rather than a lane row.
    fn cursor_into_active(dashboard: &mut Dashboard) {
        for _ in 0..2 {
            assert_eq!(
                dashboard.key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)),
                UiCommand::None,
                "moving the cursor costs the daemon nothing"
            );
        }
    }

    /// One pane's queue as the daemon would report it.
    fn pane_queue(pane_id: &str, entries: usize, auto_feed: bool) -> PaneQueueSnapshot {
        PaneQueueSnapshot {
            workspace_id: "w".into(),
            pane_id: pane_id.into(),
            run_id: None,
            auto_feed,
            awaiting_ack: false,
            holding_because: None,
            entries: (1..=entries)
                .map(|entry_id| crate::protocol::QueueEntrySnapshot {
                    entry_id: entry_id as u64,
                    label: format!("task {entry_id}"),
                    preview: "rewrite the parser".into(),
                    bytes: 18,
                })
                .collect(),
        }
    }

    /// A board pane holding the focus, with two agents in the lane.
    ///
    /// Two rather than one because every claim worth making about arming is about *which* pane it
    /// reached: a lane with a single row cannot tell arming the selected pane apart from arming
    /// whichever pane happened to be first.
    fn board_pane_with_two_agents() -> Dashboard {
        let mut dashboard = dashboard_with_a_board_pane();
        // The board has the canvas to itself, which is how a board pane is actually used and
        // what a card needs to have room for its second line: five columns across half a
        // terminal leave twelve cells each, and a card that narrow says only what its glyph
        // says. The runs stay — a run is a run whether or not its pane is on this canvas.
        let workspace = &mut dashboard.layout.workspaces[0];
        workspace.focused_pane_id = "b".into();
        workspace.panes.remove("a");
        workspace.root = LayoutNode::Pane {
            pane_id: "b".into(),
        };
        let mut second = snapshot();
        second.run_id = "run_2".into();
        second.workspace_id = "w".into();
        second.pane_id = "d".into();
        second.external_task_ref = "9".into();
        dashboard.runs.push(second);
        dashboard.agents.insert(
            "run_2".into(),
            (Some(AgentKind::Claude), AgentState::Working),
        );
        dashboard
    }

    #[test]
    fn an_active_entry_shows_its_panes_queue_depth_and_whether_it_is_armed() {
        // The two placeholders the lane shipped with, now on the card itself. Queue depth lives
        // only in the daemon, so unlike agent state there is nothing on the client that could
        // have implied it.
        let mut dashboard = board_pane_with_two_agents();
        dashboard.set_queues(
            vec![pane_queue("a", 3, true), pane_queue("d", 0, false)],
            false,
        );

        let rows = dashboard.live_runs();
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(
            (rows[0].pane_id, rows[0].queued, rows[0].auto_feed),
            ("a", 3, true)
        );
        assert_eq!(
            (rows[1].pane_id, rows[1].queued, rows[1].auto_feed),
            ("d", 0, false)
        );

        // Both entries at once, which is what the column is for, so this is drawn wide enough
        // for two full liveness lines rather than read out of the pane's footer.
        let terminal = render_terminal(&mut dashboard, 400, 40);
        let active = board_column(&terminal, "ACTIVE");
        assert!(active.contains("3 queued"), "{active:?}");
        // Zero is drawn too. "Is anything waiting behind this agent" is the question somebody
        // about to arm a pane is asking, and a board that goes quiet on an empty queue cannot
        // answer it.
        assert!(active.contains("0 queued"), "{active:?}");
        assert!(active.contains("armed"), "{active:?}");
    }

    #[test]
    fn a_queue_that_is_holding_says_why_it_is_holding() {
        // Why `holding_because` exists at all. An armed pane with four prompts behind it that
        // feeds nothing is indistinguishable from a broken one until it says which guard it is
        // sitting behind, and the daemon's own sentence is the only one that knows.
        let mut dashboard = board_pane_with_two_agents();
        let mut holding = pane_queue("a", 4, true);
        holding.holding_because = Some(crate::queue::DISARMED_BY_RESTART.into());
        dashboard.set_queues(vec![holding], false);
        // Onto the entry that is stuck. The reason is seventy characters and a card is thirty,
        // so the pane's footer is where it fits whole — which is also where the cursor is about
        // to arm something, and therefore where a person is looking.
        render_to_string(&mut dashboard, 400, 40);
        cursor_into_active(&mut dashboard);

        let frame = render_to_string(&mut dashboard, 400, 40);
        assert!(
            frame.contains(crate::queue::DISARMED_BY_RESTART),
            "the daemon's own words, not a house paraphrase: {frame:?}"
        );

        // And not on a pane with nothing to hold: auto-feed declines on an empty queue for the
        // uninteresting reason that there was nothing to feed, and printing that on every idle
        // row would bury the one row that is actually stuck.
        let mut empty = pane_queue("a", 0, true);
        empty.holding_because = Some(crate::queue::DISARMED_BY_RESTART.into());
        dashboard.set_queues(vec![empty], false);
        let frame = render_to_string(&mut dashboard, 400, 40);
        assert!(
            !frame.contains(crate::queue::DISARMED_BY_RESTART),
            "{frame:?}"
        );
    }

    #[test]
    fn a_queue_changed_event_repaints_the_board_with_no_key_fed() {
        // `dock queue add` from another terminal, and the acceptance criterion it has to satisfy:
        // it appears in the open board pane without a refresh. The client half is two halves —
        // the event marks the client dirty, and the render loop's own `refresh` re-reads the
        // listing on the way to the next frame. Neither is a keypress, and this asserts both
        // without feeding one.
        let mut dashboard = board_pane_with_two_agents();
        dashboard.set_queues(vec![pane_queue("a", 0, false)], false);
        assert!(render_to_string(&mut dashboard, 400, 40).contains("0 queued"));

        dashboard.apply_event(Event::QueueChanged {
            workspace_id: "w".into(),
            pane_id: "a".into(),
        });
        assert!(
            dashboard.take_refresh(),
            "queue depth lives only in the daemon, so nothing but going back to it would tell \
             this client the queue grew"
        );

        // What the render loop does with that flag: `refresh` asks for the listing and hands it
        // over. The frame after says the new depth, and no key was ever fed.
        dashboard.set_queues(vec![pane_queue("a", 2, false)], false);
        let frame = render_to_string(&mut dashboard, 400, 40);
        assert!(frame.contains("2 queued"), "{frame:?}");
    }

    #[test]
    fn a_paused_daemon_says_so_on_the_board_even_when_a_pane_is_armed() {
        // The daemon-wide kill switch is the one thing that makes an *armed* pane feed nothing,
        // and without it a paused daemon looks like a broken one. It used to be on the runs
        // lane's heading; with the lane gone it is the first thing in the pane's own footer,
        // which is the one line wide enough to hold it.
        let mut dashboard = board_pane_with_two_agents();
        dashboard.set_queues(vec![pane_queue("a", 2, true)], true);
        let frame = render_to_string(&mut dashboard, 400, 40);
        assert!(frame.contains("PAUSED"), "{frame:?}");
        assert!(frame.contains("every queue is held"), "{frame:?}");
        // And the pane still says it is armed, because "armed, and paused anyway" is two facts.
        assert!(frame.contains("armed"), "{frame:?}");
    }

    #[test]
    fn the_board_overlay_is_tall_enough_for_an_active_column_of_live_agents() {
        // The popup is sized to its tallest column. `ACTIVE` counts double and can hold more
        // entries than it has cards, so measuring it in cards clipped the bottom off the one
        // column that grew — which is the column this whole pass exists to fill.
        // No board pane on this canvas, so the only `ACTIVE` column on screen is the overlay's.
        let mut dashboard = bound_dashboard();
        let mut agent = snapshot();
        agent.run_id = "run_1".into();
        agent.external_task_ref = "7".into();
        dashboard.runs.push(agent);
        dashboard.agents.insert(
            "run_1".into(),
            (Some(AgentKind::Claude), AgentState::Blocked),
        );
        dashboard.set_board_tasks(
            vec![
                board_task(7, "wire the parser", "in-progress"),
                board_task(21, "audit the queue", "in-progress"),
                board_task(22, "retry the fetch", "in-progress"),
            ],
            "/repo/real/kanban/tasks".into(),
        );
        // Three cards, one of them with an agent on it: three entries, six rows, over a popup
        // that used to be sized for three *cards* and so had room for one and a half of them.
        let terminal = render_terminal(&mut dashboard, 130, 40);
        let active = board_column(&terminal, "ACTIVE");
        assert!(active.contains("#7"), "{active:?}");
        assert!(
            active.contains("#22"),
            "the last entry is drawn rather than clipped: {active:?}"
        );
    }

    #[test]
    fn a_on_an_active_entry_arms_that_entrys_pane_and_only_that_pane() {
        // Arming is the one deliberate act that lets Dock put words in front of an agent with
        // nobody watching, so the key names one pane explicitly and there is no key that arms
        // the column. `UiCommand::Request` carries exactly one request; `Requests` — the batch
        // form — is deliberately not what this returns. Only the way the target is chosen
        // changed when the lane went: it is the grid's own cursor now.
        let mut dashboard = board_pane_with_two_agents();
        dashboard.set_queues(
            vec![pane_queue("a", 1, false), pane_queue("d", 1, false)],
            false,
        );
        render_to_string(&mut dashboard, 400, 40);
        cursor_into_active(&mut dashboard);

        let asked = dashboard.key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        let UiCommand::Request(request) = asked else {
            panic!("`a` on an ACTIVE entry must ask the daemon to arm, got {asked:?}");
        };
        assert_eq!(
            *request,
            Request::Queue(QueueRequest::SetAuto {
                workspace_id: "w".into(),
                pane_id: "a".into(),
                enabled: true,
            }),
            "the entry under the cursor, which is the blocked agent at the top of the column"
        );

        // The other pane is untouched: nothing local arms anything, and the listing only changes
        // when the daemon says it did.
        assert!(!dashboard.live_runs()[1].auto_feed);
    }

    #[test]
    fn a_outside_active_says_what_it_arms_rather_than_arming_whatever_is_nearest() {
        // The cursor is one cursor over the whole grid, so it can be on a backlog card — and a
        // backlog card has no agent. Falling back to "the first agent" would make the one
        // deliberate act in Dock happen to a pane the user was not pointing at.
        let mut dashboard = board_pane_with_two_agents();
        dashboard.set_queues(vec![pane_queue("a", 1, false)], false);
        render_to_string(&mut dashboard, 400, 40);

        assert_eq!(
            dashboard.key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
            UiCommand::None,
            "nothing is asked of the daemon"
        );
        assert!(
            dashboard
                .error
                .as_deref()
                .is_some_and(|message| message.contains("ACTIVE")),
            "{:?}",
            dashboard.error
        );
        assert!(!dashboard.live_runs()[0].auto_feed);
    }

    #[test]
    fn the_board_cursor_chooses_which_pane_a_arms_and_a_re_sort_never_slides_another_under_it() {
        // The selection model, and the reason it names a pane rather than an index: `ACTIVE`
        // sorts blocked agents to the top, so a position would quietly point somewhere else the
        // moment an agent changed state. This is the property the deleted lane cursor existed
        // for, and it is now the property of the one cursor over the whole grid.
        let mut dashboard = board_pane_with_two_agents();
        dashboard.set_queues(
            vec![pane_queue("a", 1, false), pane_queue("d", 1, false)],
            false,
        );
        render_to_string(&mut dashboard, 400, 40);
        cursor_into_active(&mut dashboard);

        assert_eq!(
            dashboard.key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)),
            UiCommand::None,
            "moving the cursor costs the daemon nothing"
        );
        let asked = dashboard.key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        let UiCommand::Request(request) = asked else {
            panic!("expected an arming request, got {asked:?}");
        };
        assert_eq!(
            *request,
            Request::Queue(QueueRequest::SetAuto {
                workspace_id: "w".into(),
                pane_id: "d".into(),
                enabled: true,
            })
        );

        // The column re-orders under the cursor when the agent it names starts working, and the
        // cursor goes with the pane rather than staying over the second entry.
        dashboard.apply_event(Event::AgentStateChanged {
            run_id: "run_1".into(),
            agent: Some(AgentKind::Claude),
            state: AgentState::Working,
        });
        dashboard.apply_event(Event::AgentStateChanged {
            run_id: "run_2".into(),
            agent: Some(AgentKind::Claude),
            state: AgentState::Blocked,
        });
        let asked = dashboard.key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        let UiCommand::Request(request) = asked else {
            panic!("expected an arming request, got {asked:?}");
        };
        assert_eq!(
            *request,
            Request::Queue(QueueRequest::SetAuto {
                workspace_id: "w".into(),
                pane_id: "d".into(),
                enabled: true,
            }),
            "still the pane the user chose, which is now the top entry rather than the second"
        );
    }

    #[test]
    fn a_on_an_armed_pane_disarms_it() {
        // A toggle read from the daemon's listing rather than from anything this client decided,
        // so a pane armed from `dock queue arm` in another terminal is disarmed by the same key.
        let mut dashboard = board_pane_with_two_agents();
        dashboard.set_queues(vec![pane_queue("a", 2, true)], false);
        render_to_string(&mut dashboard, 400, 40);
        cursor_into_active(&mut dashboard);

        let asked = dashboard.key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        let UiCommand::Request(request) = asked else {
            panic!("expected a disarming request, got {asked:?}");
        };
        assert_eq!(
            *request,
            Request::Queue(QueueRequest::SetAuto {
                workspace_id: "w".into(),
                pane_id: "a".into(),
                enabled: false,
            })
        );
    }

    #[test]
    fn a_refused_arming_reaches_the_user_in_the_daemons_own_words() {
        // The refusal is the product. A pane whose agent has never reported a state can be armed
        // as often as you like and will never feed anything, so a refusal that got swallowed
        // would leave a person watching a queue that was silently never going to fire — which is
        // strictly worse than the pane refusing to be armed at all. The message names the command
        // that fixes it, and it has to arrive whole.
        let mut dashboard = board_pane_with_two_agents();
        dashboard.set_queues(vec![pane_queue("a", 1, false)], false);
        render_to_string(&mut dashboard, 400, 40);
        cursor_into_active(&mut dashboard);
        assert!(matches!(
            dashboard.key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
            UiCommand::Request(_)
        ));

        dashboard.apply_queue_response(Response::Error {
            code: crate::protocol::ErrorCode::QueueRefused,
            message: crate::queue::ARM_WITHOUT_REPORTED_STATE.into(),
        });
        let frame = render_to_string(&mut dashboard, 240, 40);
        assert!(
            frame.contains(crate::queue::ARM_WITHOUT_REPORTED_STATE),
            "verbatim, not paraphrased and not truncated to a house message: {frame:?}"
        );
        assert!(
            frame.contains("dock hooks --install"),
            "the refusal names the command that makes arming possible: {frame:?}"
        );
        // And the pane stayed unarmed. A refusal that left the lane claiming otherwise would be
        // the same failure as swallowing it.
        assert!(!dashboard.live_runs()[0].auto_feed);
    }

    #[test]
    fn a_board_pane_takes_the_lanes_keys_without_any_of_them_reaching_a_pty() {
        // §2.3: `pane_input` and `send_to_pane` stay unchanged for a board pane. A board has no
        // run and is never going to get one, so the keys are intercepted ahead of `send_to_pane`
        // and nothing a board takes can become `PaneInput` however it is encoded.
        let mut dashboard = board_pane_with_two_agents();
        dashboard.set_queues(vec![pane_queue("a", 1, false)], false);
        render_to_string(&mut dashboard, 200, 40);

        for code in [
            KeyCode::Char('x'),
            KeyCode::Char('j'),
            KeyCode::Enter,
            KeyCode::Char('a'),
        ] {
            let outcome = dashboard.key(KeyEvent::new(code, KeyModifiers::NONE));
            assert!(
                !matches!(outcome, UiCommand::PaneInput(_)),
                "{code:?} reached a PTY from a board pane: {outcome:?}"
            );
        }

        // And the prefix still commands the dashboard from inside a board, because the keymap
        // spends it before any of this is reached.
        let asked = command(&mut dashboard, KeyCode::Char('k'));
        assert_eq!(asked, UiCommand::LoadBoard);
    }

    #[test]
    fn ctrl_b_shift_b_asks_for_a_split_whose_new_half_is_a_board() {
        // The board overlay keeps `Ctrl+B k`, so requirement 4 costs nothing: the shifted key is
        // free and the two surfaces are one keystroke apart.
        let mut dashboard = bound_dashboard();
        render_to_string(&mut dashboard, 100, 30);
        let asked = command(&mut dashboard, KeyCode::Char('B'));
        let UiCommand::Request(request) = asked else {
            panic!("Ctrl+B B must ask for a split, got {asked:?}");
        };
        let Request::Workspace(WorkspaceRequest::Split { kind, axis, .. }) = *request else {
            panic!("expected a split request");
        };
        assert_eq!(kind, PaneKind::Board);
        // Full width: a board is five columns wide before it is anything else.
        assert_eq!(axis, SplitAxis::Horizontal);
        // And the local layout already says so, so the frame painted before the daemon answers
        // shows a board rather than an empty terminal.
        let workspace = &dashboard.layout.workspaces[0];
        assert_eq!(
            workspace.panes[&workspace.focused_pane_id].kind,
            PaneKind::Board
        );
    }

    #[test]
    fn a_board_pane_comes_back_a_board_when_the_dashboard_reopens_and_reads_the_board_itself() {
        // Quitting the TUI and reopening it means a fresh `Dashboard` filled from the daemon's
        // layout, so the kind has to survive that snapshot as well as the file on disk.
        let dashboard = dashboard_with_a_board_pane();
        let wire = serde_json::to_string(&dashboard.layout).expect("layouts go over the socket");
        let reopened: LayoutSnapshot = serde_json::from_str(&wire).expect("and come back");
        assert!(reopened.workspaces[0].panes["b"].is_board());

        // And then it has to fill itself in. The board is files only the client can see, and
        // until now the only thing that ever read them was the overlay's key — so a board pane
        // restored from a previous session would have sat there empty until somebody pressed it.
        let mut fresh = Dashboard {
            layout: reopened,
            ..Dashboard::default()
        };
        assert!(fresh.board_pane_needs_load());
        fresh.set_board_pane_tasks(
            vec![board_task(9, "write the docs", "backlog")],
            "/repo/real/kanban/tasks".into(),
        );
        assert!(
            !fresh.board_pane_needs_load(),
            "and asks exactly once, not once per frame"
        );
        assert!(render_to_string(&mut fresh, 160, 40).contains("#9"));

        // A canvas with no board on it never reads the board at all.
        assert!(!bound_dashboard().board_pane_needs_load());
    }

    #[test]
    fn a_board_pane_announces_no_geometry_and_takes_no_keystrokes() {
        // Two of the four sites this design claims need no change at all. `queue_resize` returns
        // early on `run_id: None` and a board's is permanently `None`, so nothing is announced;
        // `send_to_pane` drops input for a pane with no run, so a focused board swallows keys
        // rather than earning one daemon error per character straight into the footer.
        let mut dashboard = dashboard_with_a_board_pane();
        dashboard.layout.workspaces[0].focused_pane_id = "b".into();
        render_to_string(&mut dashboard, 160, 40);
        assert!(
            dashboard
                .take_pending_resizes()
                .iter()
                .all(|(_, pane_id, _, _)| pane_id != "b"),
            "a pane with no PTY has no geometry to announce"
        );
        assert_eq!(
            dashboard.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            UiCommand::None
        );
    }

    #[test]
    fn a_needs_input_card_is_drawn_and_reachable_with_the_arrows() {
        // `needs-input` is declared by this repository's own `kanban/config.yml` and has never
        // been in `board::STATUSES`. `BoardView` learned to take the union of the two, but the
        // dashboard still resolved its columns and its `<`/`>` through the constant — so the
        // column was not drawn, the card was in no column at all, and the one thing a person
        // could do about a blocked card was the one thing the board refused to do.
        let root = std::env::temp_dir().join(format!("dock-needs-input-{}", std::process::id()));
        let dir = root.join("tasks");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("007-blocked.md"),
            "---\nid: 7\ntitle: 'waiting on a person'\nstatus: needs-input\npriority: high\n---\n\n# Outcome\n\nsay something\n",
        )
        .unwrap();

        let mut dashboard = bound_dashboard();
        dashboard.set_board_tasks(crate::board::load(&dir), dir.clone());
        dashboard.board.as_mut().unwrap().writable = true;

        let frame = render_to_string(&mut dashboard, 130, 32);
        assert!(
            frame.contains("NEEDS-INPUT"),
            "the column a card is actually in must be drawn: {frame:?}"
        );
        assert!(frame.contains("#7"), "and the card in it: {frame:?}");

        // The only card on the board, so the cursor opens on it.
        assert_eq!(
            dashboard.board.as_ref().unwrap().view.status(),
            Some("needs-input")
        );
        // `<` moves it back into a column the constant does know, which is the half that was
        // impossible: the card could be put here by hand and never taken out again.
        dashboard.key(KeyEvent::new(KeyCode::Char('<'), KeyModifiers::NONE));
        assert_eq!(crate::board::load(&dir)[0].status, "done");
        assert_eq!(
            dashboard.board.as_ref().unwrap().view.status(),
            Some("done"),
            "the cursor follows the card it just moved"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_board_is_drawn_as_columns_of_cards_with_their_counts() {
        let mut dashboard = bound_dashboard();
        dashboard.set_board_tasks(
            vec![
                board_task(11, "Repository-optional runtime", "review"),
                board_task(12, "Dashboard real agent dispatch", "in-progress"),
                board_task(13, "Write the docs", "backlog"),
            ],
            crate::board::tasks_dir("", "workspace_1").expect("a workspace board"),
        );
        let frame = render_to_string(&mut dashboard, 130, 32);
        // The shape is the information: where work has piled up, what is in flight, what waits on
        // a person. A list sorted by status shows none of that at a glance.
        // `ACTIVE` rather than `IN-PROGRESS`: the heading is the one thing that changed, because
        // the column holds live agents with no card as well as the cards that are in progress.
        // The status on disk and in `board::STATUSES` is still `in-progress`.
        for column in ["BACKLOG", "TODO", "ACTIVE", "REVIEW", "DONE"] {
            assert!(frame.contains(column), "missing column {column}: {frame:?}");
        }
        assert!(!frame.contains("IN-PROGRESS"), "{frame:?}");
        assert_eq!(
            dashboard.board_tasks[1].status, "in-progress",
            "and nothing about the heading touched the card"
        );
        assert!(frame.contains("#11"), "{frame:?}");
        assert!(frame.contains("#12"), "{frame:?}");
        // It opens on the leftmost column holding anything, which here is backlog.
        assert_eq!(
            dashboard.board.as_ref().unwrap().view.status(),
            Some("backlog")
        );
    }

    #[test]
    fn moving_a_card_into_active_writes_in_progress_because_active_is_only_a_heading() {
        // The one risk in renaming a column: a display name leaking into the write path. `<` and
        // `>` walk `board.view.statuses()`, which is the board's own columns and still holds
        // `in-progress` — so the heading changed and nothing else did. A file with
        // `status: active` in it would be a file `kanban-md` has never heard of.
        let root = std::env::temp_dir().join(format!("dock-heading-{}", std::process::id()));
        let dir = root.join("tasks");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&dir).unwrap();
        crate::board::create(&dir, "wire the parser").expect("seed a task");

        let mut dashboard = bound_dashboard();
        dashboard.set_board_tasks(crate::board::load(&dir), dir.clone());
        dashboard.board.as_mut().unwrap().writable = true;
        let frame = render_to_string(&mut dashboard, 130, 32);
        assert!(frame.contains("ACTIVE"), "{frame:?}");

        // Backlog to todo to in-progress, which is two columns to the right of where it started.
        dashboard.key(KeyEvent::new(KeyCode::Char('>'), KeyModifiers::NONE));
        dashboard.key(KeyEvent::new(KeyCode::Char('>'), KeyModifiers::NONE));
        assert_eq!(crate::board::load(&dir)[0].status, "in-progress");
        assert_eq!(
            dashboard.board.as_ref().unwrap().view.status(),
            Some("in-progress"),
            "and the cursor followed the card into the column it is actually in"
        );
        assert!(crate::board::STATUSES.contains(&"in-progress"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_active_entry_that_is_a_running_agent_has_nothing_for_the_move_keys_to_move() {
        // `ACTIVE` holds two kinds of thing and only one of them is a card. `>` on a live agent
        // used to be impossible because live agents were in a lane with no `>`; now that they
        // share a column with cards, the move keys have to say so rather than moving whatever
        // card the view's own row happened to be sitting over.
        let root = std::env::temp_dir().join(format!("dock-loose-{}", std::process::id()));
        let dir = root.join("tasks");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&dir).unwrap();
        crate::board::create(&dir, "wire the parser").expect("seed a task");
        let id = crate::board::load(&dir)[0].id;
        crate::board::set_status(&dir, id, "in-progress").expect("dispatch it");

        let mut dashboard = board_pane_with_two_agents();
        dashboard.runs[0].external_task_ref = id.to_string();
        dashboard.set_board_tasks(crate::board::load(&dir), dir.clone());
        dashboard.board.as_mut().unwrap().writable = true;
        render_to_string(&mut dashboard, 400, 40);

        // The cursor opens on the only column holding a card, which is `ACTIVE`, and the second
        // entry there is the agent that has no card at all.
        dashboard.key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(
            dashboard.key(KeyEvent::new(KeyCode::Char('>'), KeyModifiers::NONE)),
            UiCommand::None
        );
        assert_eq!(
            dashboard.error.as_deref(),
            Some("that is a running agent, not a card")
        );
        assert_eq!(
            crate::board::load(&dir)[0].status,
            "in-progress",
            "and the card the cursor was not on stayed exactly where it was"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn taking_a_task_asks_for_a_worktree_and_an_agent_rather_than_switching_anything_locally() {
        let mut dashboard = dashboard_with_agents(&[AdapterId::Amp, AdapterId::ClaudeCode]);
        dashboard.set_board_tasks(
            vec![board_task(12, "Dashboard real agent dispatch", "review")],
            "/repo/real/kanban/tasks".into(),
        );
        render_to_string(&mut dashboard, 100, 30);
        let taken = dashboard.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let UiCommand::DispatchTask(task) = taken else {
            panic!("taking a task must dispatch it, got {taken:?}");
        };
        assert_eq!(task.task_id, 12);
        assert_eq!(task.title, "Dashboard real agent dispatch");
        assert_eq!(task.pane_id, "a");
        // Never the fixture, whatever sits first in the profile list: it is a test stub, and a
        // dashboard whose launch form had never been opened used to dispatch every task to it.
        assert_ne!(task.adapter, AdapterId::Fixture);
        assert_eq!(Some(task.adapter.clone()), dashboard.dispatch_adapter());
        assert!(dashboard.picker.is_none());
    }

    #[test]
    fn a_task_that_left_the_board_while_it_was_open_reports_rather_than_dispatching() {
        let mut dashboard = bound_dashboard();
        dashboard.set_board_tasks(
            vec![board_task(12, "Going away", "review")],
            "/repo/real/kanban/tasks".into(),
        );
        render_to_string(&mut dashboard, 100, 30);
        // The board is files on disk that anything may rewrite between reading and choosing.
        dashboard.board_tasks.clear();
        assert_eq!(
            dashboard.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            UiCommand::None
        );
        assert_eq!(
            dashboard.error.as_deref(),
            Some("that task is no longer on the board")
        );
    }

    #[test]
    fn an_empty_board_opens_and_can_take_its_first_task() {
        let mut dashboard = bound_dashboard();
        dashboard.set_board_tasks(
            Vec::new(),
            crate::board::tasks_dir("", "workspace_1").expect("a workspace board"),
        );
        // An empty board is the normal first state of every board, not an error.
        assert!(dashboard.board.is_some(), "an empty board still opens");
        assert_eq!(dashboard.error, None);
        let frame = render_to_string(&mut dashboard, 130, 32);
        assert!(
            frame.contains("n new"),
            "the way in is on screen: {frame:?}"
        );

        dashboard.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        for character in "buy milk".chars() {
            dashboard.key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        assert_eq!(
            dashboard.board.as_ref().unwrap().composing.as_deref(),
            Some("buy milk"),
            "n starts a title rather than being swallowed as navigation"
        );
        let composing = render_to_string(&mut dashboard, 130, 32);
        assert!(composing.contains("buy milk"), "{composing:?}");
    }

    #[test]
    fn a_repository_board_is_readable_but_offers_no_way_to_change_it() {
        let mut dashboard = bound_dashboard();
        dashboard.set_board_tasks(
            vec![board_task(1, "Owned elsewhere", "backlog")],
            "/repo/real/kanban/tasks".into(),
        );
        assert!(dashboard.board.is_some());
        assert!(!dashboard.board.as_ref().unwrap().writable);
        let frame = render_to_string(&mut dashboard, 130, 32);
        assert!(frame.contains("kanban-md owns this board"), "{frame:?}");

        // The controls that would write are refused rather than silently doing nothing.
        dashboard.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(dashboard.board.as_ref().unwrap().composing.is_none());
        assert!(
            dashboard
                .error
                .as_deref()
                .is_some_and(|message| message.contains("kanban-md")),
            "{:?}",
            dashboard.error
        );
    }

    #[test]
    fn dock_refuses_to_write_a_task_into_a_repositorys_own_board() {
        let mut dashboard = bound_dashboard();
        dashboard.set_board_tasks(Vec::new(), "/repo/real/kanban/tasks".into());
        assert_eq!(dashboard.create_task("something"), UiCommand::None);
        assert!(
            dashboard
                .error
                .as_deref()
                .is_some_and(|message| message.contains("kanban-md")),
            "{:?}",
            dashboard.error
        );
    }

    fn git_facts() -> GitFacts {
        GitFacts {
            worktree: std::path::PathBuf::from("/repo/real"),
            branch: "dock/task-12".into(),
            base_sha: "abc".into(),
            head_sha: "def".into(),
            status_entries: 2,
            changed_files: 1,
            insertions: 3,
            deletions: 1,
        }
    }

    /// Built with `concat!` rather than line continuations: `cargo fmt` collapses a continued
    /// literal onto one line and bakes the indentation into the string, which silently turns every
    /// diff line into a context line.
    const SAMPLE_DIFF: &str = concat!(
        "diff --git a/src/lib.rs b/src/lib.rs\n",
        "index 111..222 100644\n",
        "--- a/src/lib.rs\n",
        "+++ b/src/lib.rs\n",
        "@@ -1,3 +1,5 @@\n",
        "+added line\n",
        "-removed line\n",
        " context line\n",
    );

    #[test]
    fn the_git_key_asks_for_the_worktrees_state_rather_than_reading_it_inline() {
        let mut dashboard = bound_dashboard();
        render_to_string(&mut dashboard, 100, 30);
        assert_eq!(
            command(&mut dashboard, KeyCode::Char('g')),
            UiCommand::LoadGit
        );
    }

    #[test]
    fn the_git_overlay_shows_the_branch_its_counts_and_the_diff() {
        let mut dashboard = bound_dashboard();
        dashboard.set_git(git_facts(), SAMPLE_DIFF.into());
        let frame = render_to_string(&mut dashboard, 110, 34);
        assert!(frame.contains("GIT"), "{frame:?}");
        assert!(frame.contains("dock/task-12"), "{frame:?}");
        assert!(frame.contains("+3"), "{frame:?}");
        assert!(frame.contains("added line"), "{frame:?}");
        assert!(frame.contains("@@ -1,3 +1,5 @@"), "{frame:?}");
    }

    #[test]
    fn added_and_removed_lines_are_coloured_by_dock_rather_than_by_an_external_renderer() {
        let mut dashboard = bound_dashboard();
        dashboard.set_git(git_facts(), SAMPLE_DIFF.into());
        let mut terminal = Terminal::new(TestBackend::new(110, 34)).unwrap();
        terminal.draw(|frame| dashboard.render(frame)).unwrap();
        let theme = Theme::warm();
        let buffer = terminal.backend().buffer();
        // Find the two rows by their first cell's glyph, then assert the palette they were
        // painted with is Dock's own.
        let mut added = None;
        let mut removed = None;
        for y in 0..34 {
            for x in 0..109 {
                let cell = &buffer[(x, y)];
                if cell.symbol() == "+" && buffer[(x + 1, y)].symbol() == "a" {
                    added = Some(cell.fg);
                }
                if cell.symbol() == "-" && buffer[(x + 1, y)].symbol() == "r" {
                    removed = Some(cell.fg);
                }
            }
        }
        assert_eq!(added, Some(theme.done), "an added line uses the done token");
        assert_eq!(
            removed,
            Some(theme.blocked),
            "a removed line uses the blocked token"
        );
        assert_ne!(theme.done, theme.blocked);
    }

    #[test]
    fn scrolling_the_diff_saturates_at_both_ends() {
        let mut dashboard = bound_dashboard();
        let long: String = (0..50).map(|n| format!("+line {n}\n")).collect();
        dashboard.set_git(git_facts(), long);
        render_to_string(&mut dashboard, 110, 34);
        dashboard.key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
        assert_eq!(
            dashboard.git.as_ref().unwrap().scroll,
            0,
            "already at the top"
        );
        dashboard.key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE));
        assert_eq!(dashboard.git.as_ref().unwrap().scroll, 49);
        dashboard.key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(
            dashboard.git.as_ref().unwrap().scroll,
            49,
            "scrolling past the end would paint a blank overlay"
        );
        dashboard.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(dashboard.git.is_none());
    }

    #[test]
    fn a_clean_worktree_says_so_rather_than_showing_an_empty_box() {
        let mut dashboard = bound_dashboard();
        dashboard.set_git(git_facts(), String::new());
        let frame = render_to_string(&mut dashboard, 110, 34);
        assert!(frame.contains("nothing changed here"), "{frame:?}");
    }

    #[test]
    fn the_roster_says_which_task_each_agent_is_on() {
        let mut dashboard = bound_dashboard();
        let mut first = snapshot();
        first.run_id = "run_1".into();
        first.external_task_ref = "7".into();
        let mut second = snapshot();
        second.run_id = "run_2".into();
        second.external_task_ref = "12".into();
        dashboard.runs = vec![first, second];
        dashboard.agents.insert(
            "run_1".into(),
            (Some(AgentKind::Claude), AgentState::Blocked),
        );
        dashboard.agents.insert(
            "run_2".into(),
            (Some(AgentKind::Claude), AgentState::Working),
        );

        let rows = sidebar_rows(&mut dashboard, 100, 30).join("\n");
        // Two agents of the same kind are otherwise indistinguishable: both rows read "claude"
        // and neither says what it is doing.
        assert!(rows.contains("claude #7"), "{rows:?}");
        assert!(rows.contains("claude #12"), "{rows:?}");
        assert!(
            rows.find("#7").unwrap() < rows.find("#12").unwrap(),
            "blocked first"
        );
    }

    #[test]
    fn an_unbound_dispatch_still_remembers_which_task_it_was_for() {
        let mut dashboard = dashboard_with_agents(&[AdapterId::Amp, AdapterId::ClaudeCode]);
        dashboard.set_board_tasks(
            vec![board_task(4, "unbound work", "backlog")],
            crate::board::tasks_dir("", "workspace_1").expect("a workspace board"),
        );
        dashboard.board.as_mut().unwrap().writable = true;
        let UiCommand::DispatchTask(task) =
            dashboard.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("Enter dispatches the selected card");
        };
        // The launch request carries the task now, so the daemon records the pairing on the run
        // itself. It used to carry no task field at all, and the dashboard kept a note of what it
        // had dispatched — a note that went with it when it quit, and that a second dashboard
        // never had. What the roster reads here is the daemon's answer, not its own memory.
        let mut run = snapshot();
        run.run_id = task.run_id.clone();
        run.external_task_ref = task.task_id.to_string();
        dashboard.runs = vec![run];
        dashboard.agents.insert(
            task.run_id.clone(),
            (Some(AgentKind::Claude), AgentState::Working),
        );
        let rows = sidebar_rows(&mut dashboard, 100, 30).join("\n");
        assert!(rows.contains("claude #4"), "{rows:?}");
    }

    #[test]
    fn the_roster_says_which_workspace_each_agent_is_in() {
        // The roster is the one view that spans workspaces, and it was also the only one that
        // could tell you an agent needs you without telling you where to go and answer it.
        let mut dashboard = bound_dashboard();
        let mut run = snapshot();
        run.run_id = "run_1".into();
        run.external_task_ref = "7".into();
        dashboard.runs = vec![run];
        dashboard.agents.insert(
            "run_1".into(),
            (Some(AgentKind::Claude), AgentState::Blocked),
        );

        let rows = sidebar_rows(&mut dashboard, 100, 30).join("\n");
        // "Daily" is the name a person gave the workspace; the id it also has is what the name
        // exists to avoid, so the roster must not fall back to it while a name is available.
        assert!(rows.contains("Daily"), "{rows:?}");
        assert!(rows.contains("claude #7"), "{rows:?}");
    }

    #[test]
    fn an_agent_the_layout_does_not_place_still_lists_without_inventing_a_workspace() {
        // A run no pane holds must still appear: its state is the reason the roster exists. What
        // it must not do is name somewhere the run is not.
        let mut dashboard = bound_dashboard();
        let mut run = snapshot();
        run.run_id = "run_elsewhere".into();
        run.external_task_ref = "9".into();
        dashboard.runs = vec![run];
        dashboard.agents.insert(
            "run_elsewhere".into(),
            (Some(AgentKind::Claude), AgentState::Working),
        );

        let rows = sidebar_rows(&mut dashboard, 100, 30).join("\n");
        assert!(rows.contains("claude #9"), "{rows:?}");
        assert!(
            !rows.contains("claude #9 ·"),
            "no dangling separator: {rows:?}"
        );
        assert!(
            !rows.contains("   in "),
            "no workspace line without a workspace: {rows:?}"
        );
    }

    #[test]
    fn the_roster_says_which_agent_wants_you_rather_than_only_colouring_it() {
        let mut dashboard = bound_dashboard();
        dashboard.agents.insert(
            "run_1".into(),
            (Some(AgentKind::Claude), AgentState::Blocked),
        );
        dashboard.agents.insert(
            "run_2".into(),
            (Some(AgentKind::Codex), AgentState::Working),
        );
        let rows = sidebar_rows(&mut dashboard, 100, 30).join("\n");
        // A coloured glyph says something is true of this agent without saying what. The word is
        // the part that is readable across a room, and "needs you" is why the roster exists.
        assert!(rows.contains("needs you"), "{rows:?}");
        assert!(rows.contains("working"), "{rows:?}");
        // Blocked still sorts above working, so the agent wanting attention is the first read.
        let blocked = rows.find("needs you").expect("blocked row");
        let working = rows.find("working").expect("working row");
        assert!(blocked < working, "blocked must sort first: {rows:?}");
    }

    fn click(dashboard: &mut Dashboard, area: Rect) -> UiCommand {
        dashboard.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        })
    }

    #[test]
    fn the_tab_strip_can_add_and_rename_a_workspace_without_the_keyboard() {
        let mut dashboard = two_workspace_dashboard();
        render_to_string(&mut dashboard, 110, 30);

        let plus = dashboard.new_workspace_area.expect("the + is drawn");
        let UiCommand::Request(request) = click(&mut dashboard, plus) else {
            panic!("the + must create a workspace");
        };
        assert!(matches!(
            *request,
            Request::Workspace(WorkspaceRequest::Create { .. })
        ));

        render_to_string(&mut dashboard, 110, 30);
        let pencil = dashboard
            .rename_workspace_area
            .expect("the active tab carries a rename affordance");
        assert_eq!(click(&mut dashboard, pencil), UiCommand::None);
        // The form opens on the workspace, not the focused pane — the protocol has always
        // distinguished them, but nothing produced the workspace form until tabs were clickable.
        assert_eq!(
            dashboard.rename_form.as_ref().map(|(target, _)| *target),
            Some(RenameTarget::Workspace)
        );
        assert!(render_to_string(&mut dashboard, 110, 30).contains("Workspace name:"));
    }

    #[test]
    fn renaming_a_workspace_sends_no_pane_id_and_repaints_the_tab_immediately() {
        let mut dashboard = two_workspace_dashboard();
        render_to_string(&mut dashboard, 110, 30);
        let pencil = dashboard.rename_workspace_area.expect("rename affordance");
        click(&mut dashboard, pencil);
        for _ in 0..40 {
            dashboard.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        }
        for character in "release".chars() {
            dashboard.key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        let UiCommand::Request(request) =
            dashboard.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("Enter must send the rename");
        };
        match *request {
            Request::Workspace(WorkspaceRequest::Rename { pane_id, name, .. }) => {
                assert_eq!(pane_id, None, "a workspace rename carries no pane");
                assert_eq!(name, "release");
            }
            other => panic!("expected a rename, got {other:?}"),
        }
        assert!(render_to_string(&mut dashboard, 110, 30).contains("release"));
    }

    #[test]
    fn the_focused_pane_can_be_split_and_closed_from_its_own_border() {
        let mut dashboard = bound_dashboard();
        render_to_string(&mut dashboard, 110, 30);
        let controls: Vec<PaneControl> = dashboard
            .pane_control_areas
            .iter()
            .map(|(control, _)| *control)
            .collect();
        assert_eq!(
            controls,
            vec![
                PaneControl::SplitHorizontal,
                PaneControl::SplitVertical,
                PaneControl::Rename,
                PaneControl::Close
            ]
        );
        let (_, close) = dashboard
            .pane_control_areas
            .iter()
            .find(|(control, _)| *control == PaneControl::Close)
            .copied()
            .expect("a close control");
        let UiCommand::Request(request) = click(&mut dashboard, close) else {
            panic!("the close control must close the pane");
        };
        assert!(matches!(
            *request,
            Request::Workspace(WorkspaceRequest::Close { .. })
        ));
    }

    #[test]
    fn pane_controls_never_paint_over_a_title_that_needs_reading() {
        let mut dashboard = bound_dashboard();
        // An exited pane's title carries the key that brings it back. Burying that under a close
        // button would be the worst trade available, so the controls yield instead.
        dashboard.layout.workspaces[0]
            .panes
            .get_mut("a")
            .unwrap()
            .runtime = PaneRuntime::Exited;
        let frame = render_to_string(&mut dashboard, 140, 24);
        assert!(frame.contains("Ctrl+B R restarts"), "{frame:?}");
        // An exited pane offers no split: dividing a dead pane makes no sense, and the columns
        // it would have taken belong to the hint that brings the pane back. Rename stays,
        // because what a stopped pane was for is exactly what a person needs recorded.
        assert_eq!(
            dashboard
                .pane_control_areas
                .iter()
                .map(|(control, _)| *control)
                .collect::<Vec<_>>(),
            vec![PaneControl::Rename, PaneControl::Close]
        );
    }

    #[test]
    fn the_focused_pane_can_be_renamed_from_its_own_border() {
        let mut dashboard = bound_dashboard();
        let frame = render_to_string(&mut dashboard, 110, 30);
        // Rename sits between the splits and close, so the destructive control keeps the far
        // corner it has always had and no existing muscle memory now lands on a different verb.
        assert!(frame.contains("⇋  ⇵  ✎  ×"), "{frame:?}");
        let (_, pencil) = dashboard
            .pane_control_areas
            .iter()
            .find(|(control, _)| *control == PaneControl::Rename)
            .copied()
            .expect("a rename control");
        assert_eq!(click(&mut dashboard, pencil), UiCommand::None);
        // The pane, not the workspace: the tab strip's pencil is the one that renames a
        // workspace, and the two forms are a click apart on screen.
        assert_eq!(
            dashboard.rename_form.as_ref().map(|(target, _)| *target),
            Some(RenameTarget::Pane)
        );
        assert!(render_to_string(&mut dashboard, 110, 30).contains("Pane name: editor"));
    }

    #[test]
    fn closing_a_workspace_from_its_tab_asks_first_and_then_closes_every_pane() {
        let mut dashboard = two_workspace_dashboard_with_an_agent();
        render_to_string(&mut dashboard, 110, 30);
        let cancel = dashboard
            .close_workspace_area
            .expect("the active tab carries a close affordance");
        // One click on a three-cell target is not consent to end two panes and the agent running
        // in them, so the first click only asks.
        assert_eq!(click(&mut dashboard, cancel), UiCommand::None);
        let frame = render_to_string(&mut dashboard, 110, 30);
        // The footer names what would be lost and the keys that answer. Neither was said before,
        // so the one destructive question Dock asks was also the one it explained least.
        assert!(frame.contains("CLOSE"), "{frame:?}");
        assert!(frame.contains("1 running agent"), "{frame:?}");
        assert!(frame.contains("Esc"), "{frame:?}");

        let confirm = dashboard
            .confirm_close_workspace_area
            .expect("the armed tab carries a confirm target");
        let UiCommand::Requests(requests) = click(&mut dashboard, confirm) else {
            panic!("the confirm target must close the workspace");
        };
        // The daemon has no close-workspace operation and needs none: it drops a workspace with
        // its last pane, so every pane is named rather than the protocol being widened.
        let closed: Vec<(String, String)> = requests
            .iter()
            .map(|request| match request {
                Request::Workspace(WorkspaceRequest::Close {
                    workspace_id,
                    pane_id,
                }) => (workspace_id.clone(), pane_id.clone()),
                other => panic!("expected a pane close, got {other:?}"),
            })
            .collect();
        assert_eq!(
            closed,
            vec![
                ("w".to_owned(), "a".to_owned()),
                ("w".to_owned(), "b".to_owned())
            ]
        );
        assert_eq!(dashboard.close_workspace_armed, None);
        // Nothing is removed locally: the daemon's reply and the refresh behind it decide what
        // survived, and a client that guessed would disagree with it for one frame.
        assert_eq!(dashboard.layout.workspaces.len(), 2);
    }

    #[test]
    fn a_double_click_on_close_cancels_rather_than_destroying_the_workspace() {
        // The whole point of asking. Arming and confirming used to share one cell, so a double
        // click armed and then fired, and the workspace was gone without the question ever
        // having been visible. Cancel now keeps that cell and confirm is somewhere else.
        let mut dashboard = two_workspace_dashboard_with_an_agent();
        render_to_string(&mut dashboard, 110, 30);
        let control = dashboard.close_workspace_area.expect("close affordance");
        assert_eq!(click(&mut dashboard, control), UiCommand::None);
        render_to_string(&mut dashboard, 110, 30);
        // The same cell, pressed again the way a double click does.
        assert_eq!(click(&mut dashboard, control), UiCommand::None);
        assert_eq!(
            dashboard.close_workspace_armed, None,
            "the second press must cancel"
        );
    }

    #[test]
    fn a_workspace_with_nothing_running_closes_without_being_asked_about() {
        // Friction belongs where the loss is. A prompt on every empty workspace is one that gets
        // dismissed by reflex, and reflex is what has to fail on the workspace holding an agent.
        let mut dashboard = two_workspace_dashboard();
        render_to_string(&mut dashboard, 110, 30);
        let control = dashboard.close_workspace_area.expect("close affordance");
        let UiCommand::Requests(requests) = click(&mut dashboard, control) else {
            panic!("a workspace with no agent running closes on the first click");
        };
        assert_eq!(requests.len(), 2);
        assert_eq!(dashboard.close_workspace_armed, None);
    }

    #[test]
    fn the_keyboard_can_close_a_workspace_and_answer_the_question_it_raises() {
        // There was no keyboard path to this at all: closing a workspace was reachable only by
        // clicking a three-cell target, alone among everything Dock does.
        let mut dashboard = two_workspace_dashboard_with_an_agent();
        assert_eq!(command(&mut dashboard, KeyCode::Char('X')), UiCommand::None);
        assert_eq!(dashboard.close_workspace_armed.as_deref(), Some("w"));
        let UiCommand::Requests(requests) =
            dashboard.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("Enter answers the armed close");
        };
        assert_eq!(requests.len(), 2);
    }

    #[test]
    fn an_armed_workspace_close_is_given_up_by_any_other_click() {
        let mut dashboard = two_workspace_dashboard_with_an_agent();
        render_to_string(&mut dashboard, 110, 30);
        let close = dashboard.close_workspace_area.expect("close affordance");
        click(&mut dashboard, close);
        assert_eq!(dashboard.close_workspace_armed.as_deref(), Some("w"));

        let pencil = dashboard.rename_workspace_area.expect("rename affordance");
        click(&mut dashboard, pencil);
        // The neighbouring control disarms like any other click: the two presses that destroy a
        // workspace have to be adjacent, or a stale arming turns an ordinary click into a close.
        assert_eq!(dashboard.close_workspace_armed, None);
        let frame = render_to_string(&mut dashboard, 110, 30);
        assert!(!frame.contains("CLOSE \u{201c}"), "{frame:?}");
    }

    #[test]
    fn an_armed_workspace_close_is_given_up_by_escape_and_by_leaving_the_workspace() {
        let mut dashboard = two_workspace_dashboard_with_an_agent();
        render_to_string(&mut dashboard, 110, 30);
        let close = dashboard.close_workspace_area.expect("close affordance");
        click(&mut dashboard, close);
        // Esc is kept from the pane only while the question is on screen; it is the key that
        // dismisses everything else Dock asks, so it has to dismiss this too.
        assert_eq!(
            dashboard.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            UiCommand::None
        );
        assert_eq!(dashboard.close_workspace_armed, None);

        render_to_string(&mut dashboard, 110, 30);
        let close = dashboard.close_workspace_area.expect("close affordance");
        click(&mut dashboard, close);
        command(&mut dashboard, KeyCode::Char('.'));
        // The confirmation was asked about the workspace that was on screen, and the tab it was
        // drawn on is no longer the active one.
        assert_eq!(dashboard.close_workspace_armed, None);
        assert_eq!(dashboard.workspace_index, 1);
    }

    #[test]
    fn once_escape_has_no_armed_close_to_answer_it_goes_back_to_the_pane() {
        let mut dashboard = bound_dashboard();
        dashboard.runs.push(snapshot());
        // Esc belongs to the focused pane; keeping it permanently is the failure this dashboard
        // deleted a whole input mode over, so the confirmation borrows it and gives it straight
        // back.
        assert_eq!(
            dashboard.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            UiCommand::PaneInput(vec![0x1b])
        );
    }

    #[test]
    fn help_publishes_the_pointer_controls_and_is_sized_to_fit_all_of_them() {
        let mut dashboard = dashboard();
        command(&mut dashboard, KeyCode::Char('?'));
        let text = render_to_string(&mut dashboard, 100, 40);
        for published in [
            "POINTER",
            "✎ rename",
            "× close (twice)",
            "⇋ ⇵ split",
            "Drag a divider to resize",
        ] {
            assert!(
                text.contains(published),
                "missing pointer control: {published}"
            );
        }
        // The overlay is sized to its own content, so the newest lines — the ones nobody has
        // read yet — are not the ones silently cut off the bottom.
        assert!(text.contains("Esc or ? closes help"), "{text:?}");
    }

    #[test]
    fn cycling_with_no_workspace_explains_itself_rather_than_panicking() {
        let mut dashboard = Dashboard::default();
        assert_eq!(command(&mut dashboard, KeyCode::Char('.')), UiCommand::None);
        assert_eq!(dashboard.workspace_index, 0);
        assert!(
            dashboard
                .error
                .as_deref()
                .is_some_and(|message| message.contains("unavailable"))
        );
    }

    #[test]
    fn a_workspace_switch_announces_the_newly_visible_pane_geometry() {
        let mut dashboard = two_workspace_dashboard();
        render_to_string(&mut dashboard, 100, 30);
        assert_eq!(
            dashboard.take_pending_resizes(),
            vec![("w".to_owned(), "a".to_owned(), TABBED_PANE_ROWS, PANE_COLS)]
        );
        command(&mut dashboard, KeyCode::Char('.'));
        render_to_string(&mut dashboard, 100, 30);
        // The second workspace is a single pane, so it owns the whole 72-column body.
        assert_eq!(
            dashboard.take_pending_resizes(),
            vec![("w2".to_owned(), "c".to_owned(), TABBED_PANE_ROWS, 70)],
            "a pane shown for the first time must be told its size"
        );
    }

    #[test]
    fn the_cursor_is_drawn_only_in_the_focused_pane() {
        let mut dashboard = bound_dashboard();
        dashboard.layout.workspaces[0]
            .panes
            .get_mut("b")
            .unwrap()
            .run_id = Some("run_2".into());
        // Both screens end on a newline, so each cursor sits on a blank cell and tui-term
        // draws its block symbol there rather than merely reversing an occupied cell.
        dashboard.apply_event(attach_event("run_1", b"left pane\r\n"));
        dashboard.apply_event(attach_event("run_2", b"right pane\r\n"));
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| dashboard.render(frame)).unwrap();
        let focused = dashboard.pane_areas["a"];
        let unfocused = dashboard.pane_areas["b"];
        assert_eq!(dashboard.workspace().unwrap().focused_pane_id, "a");
        assert!(
            draws_cursor(&terminal, focused),
            "the focused pane must show where typing lands"
        );
        assert!(
            !draws_cursor(&terminal, unfocused),
            "a cursor in every pane makes focus unreadable"
        );
    }

    fn draws_cursor(terminal: &Terminal<TestBackend>, area: Rect) -> bool {
        let buffer = terminal.backend().buffer();
        for y in area.y + 1..area.bottom() - 1 {
            for x in area.x + 1..area.right() - 1 {
                if buffer[(x, y)].symbol() == "\u{2588}" {
                    return true;
                }
            }
        }
        false
    }

    #[test]
    fn typing_into_a_pane_with_no_run_is_dropped_rather_than_sent() {
        let mut dashboard = dashboard();
        // Pane "a" has no run, so there is no PTY to receive this and the daemon would answer
        // every character with an error that flickers through the footer.
        assert_eq!(
            dashboard.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            UiCommand::None
        );
        assert!(dashboard.error.is_none(), "a dropped key must not shout");
        dashboard.layout.workspaces[0]
            .panes
            .get_mut("a")
            .unwrap()
            .run_id = Some("run_1".into());
        assert!(matches!(
            dashboard.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            UiCommand::PaneInput(bytes) if bytes == b"x"
        ));
    }

    #[test]
    fn attach_then_delta_events_reconstruct_the_pane_screen() {
        let mut dashboard = dashboard();
        let mut source = crate::terminal::VtTerminal::new(24, 80, 0);
        source.feed(b"first line\r\n");
        dashboard.apply_event(Event::PaneAttached {
            run_id: "run_1".into(),
            revision: 1,
            rows: 24,
            cols: 80,
            scrollback_rows: 2000,
            history_from: 0,
            epoch: 1,
            screen: STANDARD.encode(source.state_bytes()),
        });
        let mut sync = crate::terminal::ScreenSync::new(24, 80);
        sync.apply(&source.state_bytes());
        source.feed(b"second line\r\n");
        let delta = sync.delta_from(&source);
        dashboard.apply_event(Event::PaneDelta {
            run_id: "run_1".into(),
            revision: 2,
            bytes: STANDARD.encode(&delta),
        });
        let rendered = dashboard.screen_text("run_1").expect("screen present");
        assert!(rendered.contains("first line"), "{rendered:?}");
        assert!(rendered.contains("second line"), "{rendered:?}");
    }

    #[test]
    fn a_revision_gap_drops_the_screen_so_the_client_re_attaches() {
        let mut dashboard = dashboard();
        dashboard.apply_event(Event::PaneAttached {
            run_id: "run_1".into(),
            revision: 1,
            rows: 24,
            cols: 80,
            scrollback_rows: 2000,
            history_from: 0,
            epoch: 1,
            screen: String::new(),
        });
        dashboard.apply_event(Event::PaneDelta {
            run_id: "run_1".into(),
            revision: 9,
            bytes: String::new(),
        });
        assert!(dashboard.screen_text("run_1").is_none());
    }

    #[test]
    fn a_re_attach_for_a_known_run_rebuilds_the_parser_at_the_announced_geometry() {
        let mut dashboard = dashboard();
        dashboard.apply_event(Event::PaneAttached {
            run_id: "run_1".into(),
            revision: 1,
            rows: 24,
            cols: 80,
            scrollback_rows: 2000,
            history_from: 0,
            epoch: 1,
            screen: String::new(),
        });
        let mut source = crate::terminal::VtTerminal::new(10, 40, 0);
        source.feed(b"seed\r\n");
        dashboard.apply_event(Event::PaneAttached {
            run_id: "run_1".into(),
            revision: 7,
            rows: 10,
            cols: 40,
            scrollback_rows: 2000,
            history_from: 0,
            epoch: 1,
            screen: STANDARD.encode(source.state_bytes()),
        });
        assert_eq!(dashboard.screens["run_1"].size(), (10, 40));

        // Fifteen lines is more than the new ten-row screen holds, so a parser rebuilt at the
        // announced geometry scrolls the earliest ones off. One still sized twenty-four rows
        // would keep every line, which is what makes this distinguish geometry rather than
        // merely content: a shorter screen cannot be told from a taller one until the output
        // exceeds the shorter of the two heights.
        let mut lines = Vec::new();
        for index in 1..=15 {
            lines.extend_from_slice(format!("line {index:02}\r\n").as_bytes());
        }
        dashboard.apply_event(Event::PaneDelta {
            run_id: "run_1".into(),
            revision: 8,
            bytes: STANDARD.encode(&lines),
        });
        let rendered = dashboard.screen_text("run_1").expect("screen present");
        assert!(rendered.contains("line 15"), "{rendered:?}");
        assert!(
            !rendered.contains("line 01"),
            "a ten-row screen cannot still be holding the first of fifteen lines: {rendered:?}"
        );

        // The re-seed adopted the daemon's revision, which never restarts across a re-seed, so
        // the deltas above were contiguous rather than read as a gap and dropped.
        assert_eq!(dashboard.revisions.get("run_1"), Some(&8));
    }

    /// Not a regression test: this is a characterisation guard on `vt100` itself.
    ///
    /// A scrolled viewport is an offset counted from the *bottom* of history, so appending
    /// rows underneath it should, in principle, need something to keep it pointed at the same
    /// content. Investigating this exact failure mode turned up that `vt100` already does that
    /// internally — `Grid::scroll_up` (`grid.rs:571-574` in `vt100` 0.16.2) advances
    /// `scrollback_offset` by one for every row it pushes into scrollback while the offset is
    /// non-zero — so no compensation code was added here; adding the naive fix on top of it
    /// was tried and confirmed to double-count that same growth and slide the viewport the
    /// *other* way (see `task-6-report.md` for the probe).
    ///
    /// `src/terminal/mod.rs:14` names `PaneScreen` as a swap point for the terminal engine
    /// and calls out `rio-vt` as a candidate replacement. Nothing else in this codebase
    /// pins the behaviour this test pins: swap the engine for one that does not self-adjust
    /// the scroll offset on append, and every scrolled pane would silently slide under live
    /// output again, with no other test positioned to catch it. Keep this test red-line: if
    /// it ever starts failing, the replacement engine needs its own compensation added to the
    /// `PaneDelta` arm of `Dashboard::apply_event`, which is exactly the fix this task
    /// originally set out to write.
    #[test]
    fn output_arriving_under_a_scrolled_pane_does_not_slide_it_downwards() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event("run_1", b""));
        for line in 0..50 {
            dashboard.apply_event(delta_event(
                "run_1",
                line + 2,
                format!("line {line}\r\n").as_bytes(),
            ));
        }
        dashboard.scroll_pane("run_1", 10);
        let pinned = dashboard.screens["run_1"].screen().contents();
        for line in 50..60 {
            dashboard.apply_event(delta_event(
                "run_1",
                line + 2,
                format!("line {line}\r\n").as_bytes(),
            ));
        }
        assert_eq!(
            dashboard.screens["run_1"].screen().contents(),
            pinned,
            "a person reading scrollback must not have it pulled out from under them"
        );
    }

    #[test]
    fn agent_state_events_are_recorded_and_layout_events_ask_for_one_refresh() {
        let mut dashboard = dashboard();
        dashboard.apply_event(Event::AgentStateChanged {
            run_id: "run_1".into(),
            agent: Some(crate::detect::AgentKind::Claude),
            state: crate::detect::AgentState::Working,
        });
        assert_eq!(
            dashboard.agents.get("run_1"),
            Some(&(
                Some(crate::detect::AgentKind::Claude),
                crate::detect::AgentState::Working
            ))
        );

        assert!(!dashboard.take_refresh());
        dashboard.apply_event(Event::LayoutChanged);
        assert!(dashboard.take_refresh());
        assert!(!dashboard.take_refresh(), "refresh must not latch on");
        assert!(dashboard.take_pending_resizes().is_empty());
    }

    /// Renders the sidebar and returns, for each row of `area`, the text actually painted there.
    fn sidebar_rows(dashboard: &mut Dashboard, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| dashboard.render(frame)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width.min(28))
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// The sidebar stops building rows the moment it runs out of sidebar, which is only a
    /// saving for as long as the rows it does build are exactly the rows a taller terminal
    /// would have put in the same places. It used to format every one of them — up to two
    /// lines per agent, across every workspace — and hand the lot to a paragraph that drew
    /// the first few and dropped the rest.
    #[test]
    fn a_roster_longer_than_the_sidebar_draws_the_rows_a_taller_terminal_would_have_drawn() {
        let mut dashboard = dashboard();
        for index in 0..40 {
            dashboard.apply_event(Event::AgentStateChanged {
                run_id: format!("run_{index}"),
                agent: Some(AgentKind::Claude),
                state: AgentState::Idle,
            });
        }
        let short = sidebar_rows(&mut dashboard, 100, 30);
        let tall = sidebar_rows(&mut dashboard, 100, 60);
        // The premise: the roster really does run off the short sidebar, so the rows being
        // compared are rows the short render had to decide not to build.
        assert!(
            !short.iter().any(|row| row.contains("LAUNCH AGENT")),
            "the roster must overflow the short sidebar or this proves nothing: {short:#?}"
        );
        assert!(
            tall.iter().any(|row| row.contains("LAUNCH AGENT")),
            "{tall:#?}"
        );
        // Rows 0 and 1 are the header and the last two are the footer; everything between them
        // is sidebar, and it must agree row for row.
        assert_eq!(
            short[2..28],
            tall[2..28],
            "short {short:#?}\ntall {tall:#?}"
        );
    }

    /// The roster reads every agent's task out of one index rather than scanning the run list
    /// once per agent, so that index has to answer exactly what a single lookup answers —
    /// including for the run whose binding is blank, the run this dashboard dispatched itself,
    /// and the run that is both.
    #[test]
    fn the_batch_task_index_answers_for_every_run_what_a_single_lookup_answers() {
        let mut dashboard = dashboard();
        let mut bound = snapshot();
        bound.run_id = "bound".into();
        bound.external_task_ref = "TASK-1".into();
        let mut blank = snapshot();
        blank.run_id = "blank".into();
        // Whitespace, not text: a binding of spaces means unbound, and an index that took it
        // literally would put a run on a task called "  ".
        blank.external_task_ref = "   ".into();
        let mut both = snapshot();
        both.run_id = "both".into();
        both.external_task_ref = "TASK-2".into();
        dashboard.runs = vec![bound, blank, both];
        let index = dashboard.tasks_by_run();
        for run_id in ["bound", "blank", "both", "never_heard_of"] {
            assert_eq!(
                dashboard.task_of(run_id).as_deref(),
                index.get(run_id).map(AsRef::as_ref),
                "the index and the single lookup disagree about {run_id}"
            );
        }
        // And the answers themselves, so agreement on a wrong answer cannot pass.
        assert_eq!(dashboard.task_of("bound").as_deref(), Some("TASK-1"));
        // A whitespace binding means unbound, and there is no client-local note to fall back to
        // any more, so the honest answer is that nothing knows rather than a remembered guess.
        assert_eq!(dashboard.task_of("blank"), None);
        assert_eq!(dashboard.task_of("both").as_deref(), Some("TASK-2"));
    }

    /// A run bound into two panes is a bug rather than two homes, so the index names the first
    /// workspace it appears in — which is the answer the roster gave when it searched for one.
    #[test]
    fn the_workspace_index_names_the_first_workspace_a_run_appears_in() {
        let mut dashboard = two_workspace_dashboard();
        let first = dashboard.layout.workspaces[0].name.clone();
        let pane = dashboard.layout.workspaces[0]
            .panes
            .values_mut()
            .next()
            .expect("the first workspace has a pane");
        pane.run_id = Some("shared".into());
        let pane = dashboard.layout.workspaces[1]
            .panes
            .values_mut()
            .next()
            .expect("the second workspace has a pane");
        pane.run_id = Some("shared".into());
        assert_eq!(
            dashboard.workspaces_by_run().get("shared").copied(),
            Some(first.as_str())
        );
    }

    /// Every clickable sidebar rectangle must sit on the row that actually carries its label.
    /// Recording rows from the logical line count while the paragraph wrapped meant a single
    /// long line pushed every rectangle below it off its own label, so the pointer hit the
    /// wrong control or nothing at all.
    #[test]
    fn sidebar_click_targets_land_on_the_rows_their_labels_are_rendered_on() {
        let mut dashboard = dashboard();
        // Longer than the 27 columns the sidebar's right border leaves, which is exactly what
        // used to wrap onto extra rows the recorded rectangles knew nothing about.
        dashboard.layout.workspaces[0].name = "a very long workspace name that overflows".into();
        let rows = sidebar_rows(&mut dashboard, 100, 30);
        let launch = dashboard.launch_area.expect("launch row");
        // Every menu rectangle has to sit on the row carrying its own label, which is the exact
        // thing a wrapped line above it used to break.
        for (command, area) in &dashboard.quick_action_areas {
            let label = QUICK_ACTIONS
                .iter()
                .find(|(_, _, candidate)| candidate == command)
                .map(|(_, label, _)| *label)
                .expect("every recorded row comes from the menu");
            assert!(
                rows[usize::from(area.y)].contains(label),
                "{command:?} rectangle at row {} but rows were {rows:#?}",
                area.y
            );
        }
        assert!(
            rows[usize::from(launch.y)].contains("LAUNCH AGENT"),
            "launch rectangle at row {} but rows were {rows:#?}",
            launch.y
        );
        // No sidebar row may carry a wrapped remainder of the workspace name: an over-long
        // label is truncated in place rather than stealing the row below it.
        assert_eq!(
            rows.iter().filter(|row| row.contains("long work")).count(),
            1,
            "{rows:#?}"
        );
        // And the rectangles still drive the real actions when clicked where they are drawn.
        assert_eq!(
            dashboard.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: launch.x + 1,
                row: launch.y,
                modifiers: KeyModifiers::NONE
            }),
            UiCommand::LoadCatalog
        );
        assert!(dashboard.launch_form.is_some());
    }

    /// A sidebar with more entries than rows cannot record a rectangle for a control it never
    /// drew: those coordinates belong to whatever the terminal shows there instead.
    #[test]
    fn a_sidebar_control_pushed_past_the_last_row_records_no_click_target() {
        let mut dashboard = dashboard();
        for index in 0..40 {
            dashboard.apply_event(Event::AgentStateChanged {
                run_id: format!("run_{index}"),
                agent: Some(AgentKind::Claude),
                state: AgentState::Idle,
            });
        }
        let rows = sidebar_rows(&mut dashboard, 100, 30);
        assert!(
            !rows.iter().any(|row| row.contains("LAUNCH AGENT")),
            "{rows:#?}"
        );
        assert_eq!(dashboard.launch_area, None);
        // A click where the unrendered row would have been must not open the launch form.
        assert_eq!(
            dashboard.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 1,
                row: 29,
                modifiers: KeyModifiers::NONE
            }),
            UiCommand::None
        );
        assert!(dashboard.launch_form.is_none());
    }

    /// The roster lists agents. A pane's ambient shell is a run, not an agent, and used to be
    /// listed under its raw run id.
    #[test]
    fn the_agent_roster_lists_only_runs_whose_agent_was_detected() {
        let mut dashboard = dashboard();
        dashboard.apply_event(Event::AgentStateChanged {
            run_id: "dock_sh_workspace_1_pane_2".into(),
            agent: None,
            state: AgentState::Idle,
        });
        let rows = sidebar_rows(&mut dashboard, 100, 30);
        assert!(
            !rows.iter().any(|row| row.contains("dock_sh_")),
            "{rows:#?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("none running")),
            "{rows:#?}"
        );

        dashboard.apply_event(Event::AgentStateChanged {
            run_id: "run_1".into(),
            agent: Some(AgentKind::Claude),
            state: AgentState::Blocked,
        });
        assert_eq!(dashboard.agents.len(), 2);
        assert_eq!(
            dashboard.agent_roster(),
            vec![(
                AgentState::Blocked,
                AgentKind::Claude.label(),
                None,
                // No workspace: this fixture binds no pane to `run_1`. The roster names where an
                // agent is when the layout places it and says nothing when it cannot, rather than
                // inventing somewhere for a run it does not hold.
                None
            )]
        );
        let rows = sidebar_rows(&mut dashboard, 100, 30);
        assert!(
            !rows.iter().any(|row| row.contains("none running")),
            "{rows:#?}"
        );
        assert!(
            !rows.iter().any(|row| row.contains("dock_sh_")),
            "{rows:#?}"
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.contains(AgentKind::Claude.label()))
                .count(),
            1,
            "{rows:#?}"
        );
    }

    /// The client half of the same defect: history has to arrive through the event stream.
    /// The daemon now sends the child's own bytes, so feeding a delta must scroll the replica
    /// exactly as feeding the pane's PTY scrolls the daemon's.
    #[test]
    fn a_delta_of_the_childs_own_bytes_gives_the_wheel_history_to_scroll_into() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event("run_1", b""));
        let mut written = Vec::new();
        for index in 1..=60 {
            written.extend_from_slice(format!("line {index}\r\n").as_bytes());
        }
        dashboard.apply_event(Event::PaneDelta {
            run_id: "run_1".into(),
            revision: 2,
            bytes: STANDARD.encode(&written),
        });
        let live = render_to_string(&mut dashboard, 100, 30);
        assert!(live.contains("line 60"), "{live}");
        assert!(
            !live.contains("line 05"),
            "the pane is not tall enough for this to prove anything"
        );

        let area = *dashboard.pane_areas.get("a").expect("pane a is rendered");
        for _ in 0..20 {
            dashboard.mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: area.x + 2,
                row: area.y + 2,
                modifiers: KeyModifiers::NONE,
            });
        }
        assert!(
            dashboard.screens["run_1"].is_scrolled(),
            "a delta of raw output must leave the replica with history to scroll into"
        );
        let scrolled = render_to_string(&mut dashboard, 100, 30);
        assert!(
            !scrolled.contains("line 60"),
            "the viewport never moved: {scrolled}"
        );
    }

    /// The attach frame carries the daemon's own retention, so the replica keeps exactly what
    /// the daemon keeps. A client guessing at the default would silently retain a different
    /// amount than the pane it is mirroring.
    #[test]
    fn the_replica_retains_exactly_the_history_the_attach_frame_announced() {
        let mut dashboard = dashboard();
        dashboard.apply_event(Event::PaneAttached {
            run_id: "run_1".into(),
            revision: 1,
            rows: 5,
            cols: 20,
            scrollback_rows: 3,
            history_from: 0,
            epoch: 1,
            screen: String::new(),
        });
        let mut written = Vec::new();
        for index in 1..=40 {
            written.extend_from_slice(format!("line {index}\r\n").as_bytes());
        }
        dashboard.apply_event(Event::PaneDelta {
            run_id: "run_1".into(),
            revision: 2,
            bytes: STANDARD.encode(&written),
        });
        let screen = dashboard.screens.get_mut("run_1").expect("screen present");
        screen.scroll_by(9_999);
        assert_eq!(
            screen.scroll_offset(),
            3,
            "the replica must retain the announced three rows, no more and no fewer"
        );
    }

    // CONTROLLER RULING C2: the brief's test used `dashboard()`, whose pane "a" has
    // `run_id: None`. Attaching a screen for "run_1" would leave the focused pane unbound and
    // the wheel event would land on empty space rather than exercising the scroll behaviour.
    // `bound_dashboard()` binds pane "a" to "run_1" so the pointer is actually over a screen.
    #[test]
    fn the_wheel_scrolls_the_pane_under_the_pointer_and_returning_to_live_resumes_following() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event("run_1", b""));
        if let Some(screen) = dashboard.screens.get_mut("run_1") {
            for index in 1..=60 {
                screen.feed(format!("line {index}\r\n").as_bytes());
            }
        }
        render_to_string(&mut dashboard, 100, 30);
        let area = *dashboard.pane_areas.get("a").expect("pane a is rendered");
        let (column, row) = (area.x + 2, area.y + 2);

        let scrolled = dashboard.mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(
            scrolled,
            UiCommand::None,
            "scrolling costs no daemon request"
        );
        assert!(
            dashboard.screens["run_1"].is_scrolled(),
            "the wheel must move the viewport into history"
        );

        for _ in 0..40 {
            dashboard.mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            });
        }
        assert!(
            !dashboard.screens["run_1"].is_scrolled(),
            "scrolling back to the bottom resumes following live output"
        );
    }

    // FINDING 2 (review of Task 4): pane "a" in `bound_dashboard()` is both focused and under
    // the pointer in the test above, so a handler that wrongly read `focused_pane_id` instead
    // of hit-testing `pane_areas` would pass it identically. Here pane "b" is bound to a second
    // run and the wheel lands on "b" while "a" stays focused, so only a pointer-pane
    // implementation scrolls the right screen.
    #[test]
    fn the_wheel_scrolls_the_pane_under_the_pointer_not_the_focused_pane() {
        let mut dashboard = bound_dashboard();
        dashboard.layout.workspaces[0]
            .panes
            .get_mut("b")
            .unwrap()
            .run_id = Some("run_2".into());
        assert_eq!(dashboard.layout.workspaces[0].focused_pane_id, "a");

        dashboard.apply_event(attach_event("run_1", b""));
        dashboard.apply_event(attach_event("run_2", b""));
        for run_id in ["run_1", "run_2"] {
            if let Some(screen) = dashboard.screens.get_mut(run_id) {
                for index in 1..=60 {
                    screen.feed(
                        format!(
                            "line {index}
"
                        )
                        .as_bytes(),
                    );
                }
            }
        }
        render_to_string(&mut dashboard, 100, 30);
        let area = *dashboard.pane_areas.get("b").expect("pane b is rendered");
        let (column, row) = (area.x + 2, area.y + 2);

        dashboard.mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        });

        assert!(
            dashboard.screens["run_2"].is_scrolled(),
            "the wheel must scroll the pane under the pointer (\"b\")"
        );
        assert!(
            !dashboard.screens["run_1"].is_scrolled(),
            "the wheel must not scroll the focused pane (\"a\") when the pointer is elsewhere"
        );
    }

    /// Raw child output, which is the only thing that gives a replica scrollback: a screen
    /// snapshot is cursor-addressed and never scrolls a row into history.
    fn history_lines(count: u32) -> Vec<u8> {
        (1..=count)
            .flat_map(|index| format!("line {index}\r\n").into_bytes())
            .collect()
    }

    /// A page-back answer that abuts the head of the replica's log, which is the only kind the
    /// daemon can send: `OutputLog::before` returns a contiguous run ending exactly at the
    /// cursor it was asked about. Built from the cursor rather than written out by hand so a
    /// test cannot accidentally describe a gap or an overlap the protocol cannot produce —
    /// which is what `apply_pane_history_response`'s `debug_assert` now says out loud.
    fn history_response(
        dashboard: &Dashboard,
        run_id: &str,
        epoch: u64,
        older: &[u8],
        complete: bool,
    ) -> Response {
        Response::PaneHistory {
            run_id: run_id.into(),
            epoch,
            from: dashboard.history[run_id].from - older.len() as u64,
            bytes: STANDARD.encode(older),
            complete,
        }
    }

    /// The payoff of the cursor an attach frame carries: once the viewport reaches the top of
    /// what this replica holds, the only place the rows above it can come from is the daemon.
    #[test]
    fn scrolling_to_the_top_of_what_a_pane_holds_asks_for_what_came_before() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event_at("run_1", b"", 4096, 1));
        dashboard.apply_event(delta_event("run_1", 2, &history_lines(60)));
        render_to_string(&mut dashboard, 100, 30);
        let area = *dashboard.pane_areas.get("a").expect("pane a is rendered");
        // Walk the viewport to the top of the replica's own history.
        for _ in 0..200 {
            dashboard.scroll_pane("run_1", 3);
        }
        let command = dashboard.mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: area.x + 2,
            row: area.y + 2,
            modifiers: KeyModifiers::NONE,
        });
        assert!(
            matches!(&command, UiCommand::Request(request)
                if matches!(request.as_ref(), Request::PaneHistory(r) if r.before == 4096)),
            "at the top of its history the client must ask for the bytes before its cursor: \
             {command:?}"
        );
    }

    /// The other half of that predicate. A wheel notch in the middle of a pane's own history
    /// is answered locally, and a client that asked the daemon on every notch would spend a
    /// two-megabyte round trip and a full parser rebuild on a scroll it already had the rows
    /// for.
    #[test]
    fn scrolling_in_the_middle_of_what_a_pane_holds_asks_for_nothing() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event_at("run_1", b"", 4096, 1));
        dashboard.apply_event(delta_event("run_1", 2, &history_lines(400)));
        render_to_string(&mut dashboard, 100, 30);
        let area = *dashboard.pane_areas.get("a").expect("pane a is rendered");
        let command = dashboard.mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: area.x + 2,
            row: area.y + 2,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(
            command,
            UiCommand::None,
            "hundreds of rows above the viewport are already held locally"
        );
    }

    #[test]
    fn the_prefix_and_page_keys_scroll_the_focused_pane() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event("run_1", b""));
        for line in 0..100 {
            dashboard.apply_event(delta_event(
                "run_1",
                line + 2,
                format!("line {line}\r\n").as_bytes(),
            ));
        }
        dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        dashboard.key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        assert!(
            dashboard.screens["run_1"].scroll_offset() > 0,
            "Ctrl+B PageUp must scroll the focused pane back"
        );
        dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        dashboard.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(
            dashboard.screens["run_1"].scroll_offset(),
            0,
            "Ctrl+B End must return the pane to following live output"
        );
    }

    #[test]
    fn a_scrolled_pane_says_so_rather_than_looking_hung() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event("run_1", b""));
        for line in 0..100 {
            dashboard.apply_event(delta_event(
                "run_1",
                line + 2,
                format!("line {line}\r\n").as_bytes(),
            ));
        }
        dashboard.scroll_pane("run_1", 12);
        let rendered = render_to_string(&mut dashboard, 80, 24);
        assert!(
            rendered.contains('▲'),
            "a pane that stopped following live output is indistinguishable from a hung agent: {rendered}"
        );
    }

    /// The marker degrades through a ladder as the pane narrows — the full sentence, then just
    /// the row count, then a bare glyph — and at every rung the pane's own title is still on
    /// screen. A pane too narrow for both must shorten the title, never erase it: this is the
    /// property `a_scrolled_pane_says_so_rather_than_looking_hung` cannot see on its own, because
    /// it only ever renders at one width.
    /// `MIN_TITLE_WIDTH` protects the two explanatory rungs, but the bare glyph is the floor
    /// the marker itself must never be dropped below — and a divider dragged all the way to one
    /// side reaches pane widths well under that floor routinely, not hypothetically. This drives
    /// the real mouse-drag path, the same one `resize_to_narrow_during_drag_clears_stale_divider_safely`
    /// uses, so the pane width is whatever `drag_ratio`'s own `MIN_PANE_WIDTH` clamp produces
    /// rather than a width chosen by the test.
    #[test]
    fn the_bare_glyph_marker_survives_a_divider_dragged_to_the_minimum_pane_width() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event("run_1", b""));
        for line in 0..100 {
            dashboard.apply_event(delta_event(
                "run_1",
                line + 2,
                format!("line {line}\r\n").as_bytes(),
            ));
        }
        dashboard.scroll_pane("run_1", 12);
        // Wide, so the divider has somewhere to go — a narrow terminal would clamp the drag
        // before it ever reached `MIN_PANE_WIDTH`.
        render_to_string(&mut dashboard, 200, 24);
        let divider = dashboard.dividers[0].area;
        dashboard.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: divider.x,
            row: divider.y,
            modifiers: KeyModifiers::NONE,
        });
        // All the way to column 0: `drag_ratio` clamps this to `MIN_PANE_WIDTH`, exactly what a
        // user dragging a divider as far as it will go produces.
        dashboard.mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 0,
            row: divider.y,
            modifiers: KeyModifiers::NONE,
        });
        let rendered = render_to_string(&mut dashboard, 200, 24);
        let area = dashboard.pane_areas["a"];
        assert!(
            area.width <= MIN_PANE_WIDTH + 2,
            "a divider dragged fully to one side should land at or near MIN_PANE_WIDTH: {area:?}"
        );
        assert!(
            rendered.contains('▲'),
            "the bare glyph must survive a pane dragged down to its minimum width, not just the \
             wider panes a terminal-size sweep happens to hit: {rendered}"
        );
    }

    /// The exact widths the review reproduced the defect at (6, 8, 10, 12 columns — all well
    /// under `MIN_PANE_WIDTH`'s neighbourhood and all reachable by setting the split ratio
    /// directly on a wide terminal, the same state a divider drag produces). At every one of
    /// them the bare glyph must survive; below the two-column floor there is nowhere left to
    /// put it and no marker is asserted.
    #[test]
    fn the_bare_glyph_marker_is_present_at_widths_where_it_previously_vanished() {
        for width in [4u16, 6, 8, 10, 12] {
            let mut dashboard = bound_dashboard();
            dashboard.apply_event(attach_event("run_1", b""));
            for line in 0..100 {
                dashboard.apply_event(delta_event(
                    "run_1",
                    line + 2,
                    format!("line {line}\r\n").as_bytes(),
                ));
            }
            dashboard.scroll_pane("run_1", 12);
            render_to_string(&mut dashboard, 300, 24);
            // A wide terminal so any ratio_milli from 0..=1000 can place pane "a" at any width
            // from 0 up to most of the terminal; searching it lands exactly on `width` rather
            // than approximating it with a hand-picked ratio that would go stale if the layout
            // arithmetic ever changed.
            let landed = (0u16..=1000).any(|ratio| {
                set_parent_ratio(&mut dashboard.layout.workspaces[0].root, "b", ratio);
                render_to_string(&mut dashboard, 300, 24);
                dashboard.pane_areas["a"].width == width
            });
            assert!(landed, "no split ratio placed pane \"a\" at width {width}");
            let rendered = render_to_string(&mut dashboard, 300, 24);
            assert!(
                rendered.contains('▲'),
                "pane width {width} (reachable by dragging a divider) lost the bare glyph \
                 entirely instead of shortening the title for it: {rendered}"
            );
        }
    }

    #[test]
    fn the_scroll_marker_shortens_the_title_rather_than_erasing_it_at_every_width() {
        let scrolled_dashboard_at = |width: u16| {
            let mut dashboard = bound_dashboard();
            dashboard.apply_event(attach_event("run_1", b""));
            for line in 0..100 {
                dashboard.apply_event(delta_event(
                    "run_1",
                    line + 2,
                    format!("line {line}\r\n").as_bytes(),
                ));
            }
            dashboard.scroll_pane("run_1", 12);
            render_to_string(&mut dashboard, width, 24)
        };

        // Narrow: only a bare glyph fits beside the title, so that is all that is asked for.
        let narrow = scrolled_dashboard_at(60);
        assert!(
            narrow.contains("editor"),
            "a narrow pane must shorten its title for the marker, not erase it: {narrow}"
        );
        assert!(
            narrow.contains('▲'),
            "even the narrowest pane that can say anything must say it stopped following: {narrow}"
        );
        assert!(
            !narrow.contains("rows"),
            "a pane too narrow for the row count must not overwrite its own title to show it anyway: {narrow}"
        );

        // Mid: room for the row count, still not the whole sentence.
        let mid = scrolled_dashboard_at(75);
        assert!(
            mid.contains("editor"),
            "a mid-width pane must still show its own title: {mid}"
        );
        assert!(
            mid.contains("rows"),
            "a mid-width pane has room for more than the bare glyph: {mid}"
        );
        assert!(
            !mid.contains("End to follow"),
            "a mid-width pane must not claim room for the full sentence it does not have: {mid}"
        );

        // Wide: room for everything, so the pane gets everything.
        let wide = scrolled_dashboard_at(110);
        assert!(
            wide.contains("editor"),
            "a wide pane has no excuse to shorten the title at all: {wide}"
        );
        assert!(
            wide.contains("End to follow"),
            "a wide pane must show the full instruction once there is room for it: {wide}"
        );
    }

    #[test]
    fn ctrl_b_page_down_scrolls_the_focused_pane_forward_toward_live_output() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event("run_1", b""));
        for line in 0..100 {
            dashboard.apply_event(delta_event(
                "run_1",
                line + 2,
                format!("line {line}\r\n").as_bytes(),
            ));
        }
        dashboard.scroll_pane("run_1", 40);
        let before = dashboard.screens["run_1"].scroll_offset();
        dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        dashboard.key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        assert!(
            dashboard.screens["run_1"].scroll_offset() < before,
            "Ctrl+B PageDown must scroll the focused pane forward, toward live output"
        );
    }

    #[test]
    fn history_that_arrives_extends_the_pane_upwards_without_moving_what_is_on_screen() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event_at("run_1", b"", 4096, 1));
        dashboard.apply_event(delta_event("run_1", 2, &history_lines(60)));
        for _ in 0..10 {
            dashboard.scroll_pane("run_1", 3);
        }
        let before = dashboard.screens["run_1"].scroll_offset();
        assert_ne!(before, 0, "the viewport moved into history at all");
        let visible = dashboard.screens["run_1"].screen().contents();
        let answer = history_response(&dashboard, "run_1", 1, b"older\r\nolder\r\nolder\r\n", true);
        dashboard.apply_pane_history_response(answer);
        assert_eq!(
            dashboard.screens["run_1"].scroll_offset(),
            before,
            "the offset is measured from the bottom, so more history above must not move the view"
        );
        assert_eq!(
            dashboard.screens["run_1"].screen().contents(),
            visible,
            "the same rows must still be on screen"
        );
    }

    /// The whole point of the exercise: output produced before this client ever attached is
    /// rows the wheel can reach. Everything else here is about not disturbing what is already
    /// on screen while that happens.
    #[test]
    fn history_that_arrives_becomes_rows_the_wheel_can_scroll_into() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event_at("run_1", b"", 4096, 1));
        dashboard.apply_event(delta_event("run_1", 2, &history_lines(60)));
        let older: Vec<u8> = (1..=40)
            .flat_map(|index| format!("ancient {index}\r\n").into_bytes())
            .collect();
        let answer = history_response(&dashboard, "run_1", 1, &older, true);
        dashboard.apply_pane_history_response(answer);
        for _ in 0..500 {
            dashboard.scroll_pane("run_1", 3);
        }
        let scrolled = render_to_string(&mut dashboard, 100, 30);
        assert!(
            scrolled.contains("ancient 2"),
            "output from before the attach must be reachable by scrolling: {scrolled}"
        );
    }

    /// The byte log has to track the parser, not just the seed. A rebuild replays the log, so
    /// a log that stopped at the attach would silently drop everything the pane has printed
    /// since — the most recent output, which is the output a person is actually looking at.
    #[test]
    fn a_rebuild_keeps_the_output_that_arrived_after_the_attach() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event_at("run_1", b"", 4096, 1));
        dashboard.apply_event(delta_event(
            "run_1",
            2,
            b"a line printed after attaching\r\n",
        ));
        let answer = history_response(&dashboard, "run_1", 1, b"older\r\n", true);
        dashboard.apply_pane_history_response(answer);
        let live = render_to_string(&mut dashboard, 100, 30);
        assert!(
            live.contains("a line printed after attaching"),
            "the rebuild must replay the deltas as well as the seed: {live}"
        );
    }

    #[test]
    fn a_pane_that_has_reached_the_oldest_retained_byte_stops_asking() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event_at("run_1", b"", 4096, 1));
        dashboard.apply_event(delta_event("run_1", 2, &history_lines(60)));
        let answer = history_response(&dashboard, "run_1", 1, b"oldest\r\n", true);
        dashboard.apply_pane_history_response(answer);
        render_to_string(&mut dashboard, 100, 30);
        let area = *dashboard.pane_areas.get("a").expect("pane a is rendered");
        for _ in 0..500 {
            dashboard.scroll_pane("run_1", 3);
        }
        assert_eq!(
            dashboard.mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: area.x + 2,
                row: area.y + 2,
                modifiers: KeyModifiers::NONE,
            }),
            UiCommand::None,
            "there is nothing older, so scrolling must not keep asking"
        );
    }

    /// The second thing that stops paging, and the one the daemon cannot tell the client
    /// about. A replica retains a fixed number of rows; once it holds that many, older bytes
    /// cannot be displayed however many of them are fetched, so fetching them is pure cost.
    #[test]
    fn a_pane_holding_every_row_it_is_allowed_to_stops_asking_for_older_bytes() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(Event::PaneAttached {
            run_id: "run_1".into(),
            revision: 1,
            rows: PANE_ROWS,
            cols: PANE_COLS,
            scrollback_rows: 3,
            history_from: 4096,
            epoch: 1,
            screen: String::new(),
        });
        dashboard.apply_event(delta_event("run_1", 2, &history_lines(60)));
        render_to_string(&mut dashboard, 100, 30);
        let area = *dashboard.pane_areas.get("a").expect("pane a is rendered");
        for _ in 0..500 {
            dashboard.scroll_pane("run_1", 3);
        }
        assert_eq!(
            dashboard.mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: area.x + 2,
                row: area.y + 2,
                modifiers: KeyModifiers::NONE,
            }),
            UiCommand::None,
            "a replica already holding its full row budget has nowhere to put older bytes"
        );
    }

    /// The cap on a client's own byte log, which is what keeps a pane that has run all week
    /// costing the same as one that just started — and the slack above it, which is what keeps
    /// the copy that enforces the cap off the path every delta takes.
    #[test]
    fn a_byte_log_is_copied_only_once_it_passes_the_mark_above_its_budget() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event_at("run_1", b"", 4096, 1));
        let seed = dashboard.history["run_1"].log.len();
        let brim = PANE_HISTORY_BYTES + PANE_HISTORY_TRIM_SLACK;
        // Fed straight to the log rather than through a delta: the point is the budget, and
        // parsing seventeen megabytes to reach it would make this test the slowest in the file.
        dashboard.retain_history_bytes("run_1", &vec![b'.'; brim - seed]);
        assert_eq!(
            dashboard.history["run_1"].log.len(),
            brim,
            "up to the mark the log is left alone, so no delta pays for a copy"
        );
        dashboard.retain_history_bytes("run_1", b".");
        assert_eq!(
            dashboard.history["run_1"].log.len(),
            PANE_HISTORY_BYTES,
            "past it, one copy takes the log all the way back to its budget"
        );
    }

    /// A trimmed log no longer knows the sequence it starts at, and cannot work it out: it
    /// holds cursor-addressed corrections as well as stream bytes, and those take up room
    /// without advancing the sequence. Rather than page from a cursor that is wrong by however
    /// many corrective bytes were dropped — which would have the daemon's next answer overlap
    /// the log and replay bytes twice — such a pane stops asking.
    #[test]
    fn a_pane_whose_byte_log_has_wrapped_stops_asking_rather_than_guessing_where_it_starts() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event_at("run_1", b"", 4096, 1));
        dashboard.apply_event(delta_event("run_1", 2, &history_lines(60)));
        for _ in 0..200 {
            dashboard.scroll_pane("run_1", 3);
        }
        assert!(
            dashboard.history_request_for("run_1").is_some(),
            "before the log wrapped this pane was ready to page"
        );
        dashboard.retain_history_bytes(
            "run_1",
            &vec![b'.'; PANE_HISTORY_BYTES + PANE_HISTORY_TRIM_SLACK + 1],
        );
        assert!(
            dashboard.history_request_for("run_1").is_none(),
            "a cursor that cannot name where its log begins must not be sent"
        );
    }

    /// The stopping condition the other three cannot express, and the one that matters most in
    /// practice. A pane in the alternate screen — vim, less, htop, and the agent TUIs that are
    /// most of what Dock runs — has no scrollback at all: `vt100` builds the alternate grid
    /// with a zero-length one, and `history_rows()` reads the *active* grid. So it answers 0
    /// forever, the row-capacity stop never trips, the headroom stop never trips, and every
    /// wheel notch fires a two-megabyte request and a full parser rebuild for rows that can
    /// never be displayed. One answer that raised nothing is proof enough.
    #[test]
    fn a_pane_in_the_alternate_screen_stops_asking_after_one_answer_that_showed_it_nothing() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event_at("run_1", b"", 4096, 1));
        // `\x1b[?1049h` switches to the alternate screen, whose grid vt100 gives no scrollback.
        let mut output = b"\x1b[?1049h".to_vec();
        output.extend_from_slice(&history_lines(60));
        dashboard.apply_event(delta_event("run_1", 2, &output));
        assert_eq!(
            dashboard
                .screens
                .get_mut("run_1")
                .expect("the pane is attached")
                .history_rows(),
            0,
            "the alternate screen retains no scrollback, which is the whole trap"
        );
        assert!(
            dashboard.history_request_for("run_1").is_some(),
            "the first ask is fair: nothing observed so far says it is pointless"
        );
        let answer = history_response(&dashboard, "run_1", 1, &history_lines(40), false);
        dashboard.apply_pane_history_response(answer);
        assert!(
            dashboard.history_request_for("run_1").is_none(),
            "an answer that added no row is proof the next one would not either"
        );
    }

    /// The other side of the stop above, and the one that would be silent if it broke: a pane
    /// whose answer *did* buy it rows must still be free to page further back, or deep history
    /// would end after exactly one chunk and look like the daemon running out of it.
    #[test]
    fn a_pane_whose_answer_added_rows_is_still_free_to_ask_for_the_chunk_before_it() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event_at("run_1", b"", 8192, 1));
        dashboard.apply_event(delta_event("run_1", 2, &history_lines(60)));
        for _ in 0..200 {
            dashboard.scroll_pane("run_1", 3);
        }
        assert!(
            dashboard.history_request_for("run_1").is_some(),
            "the first ask"
        );
        let answer = history_response(&dashboard, "run_1", 1, &history_lines(40), false);
        dashboard.apply_pane_history_response(answer);
        for _ in 0..200 {
            dashboard.scroll_pane("run_1", 3);
        }
        assert!(
            dashboard.history_request_for("run_1").is_some(),
            "an answer that added rows says the mechanism works, not that it is finished"
        );
    }

    /// Scrolling toward live output moves through rows the pane already holds, so it can never
    /// need anything older. The wheel arm asked on both directions, which put a two-megabyte
    /// round trip and a full parser rebuild on half of every wheel gesture.
    #[test]
    fn a_wheel_notch_toward_live_output_never_asks_for_older_history() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event_at("run_1", b"", 4096, 1));
        dashboard.apply_event(delta_event("run_1", 2, &history_lines(60)));
        render_to_string(&mut dashboard, 100, 30);
        let area = *dashboard.pane_areas.get("a").expect("pane a is rendered");
        let notch = |kind| MouseEvent {
            kind,
            column: area.x + 2,
            row: area.y + 2,
            modifiers: KeyModifiers::NONE,
        };
        for _ in 0..40 {
            dashboard.scroll_pane("run_1", 3);
        }
        assert!(
            matches!(
                dashboard.mouse(notch(MouseEventKind::ScrollUp)),
                UiCommand::Request(_)
            ),
            "at the top of what it holds, a notch back does ask — otherwise this proves nothing"
        );
        assert_eq!(
            dashboard.mouse(notch(MouseEventKind::ScrollDown)),
            UiCommand::None,
            "a notch toward live output must not ask, however near the top the pane is"
        );
    }

    /// A frozen pane paints a clone taken before the request could ever be answered, so a
    /// rebuild of the live parser cannot add a row to what the user is looking at. Without
    /// this the wheel would fire a two-megabyte request and a full rebuild on every notch for
    /// as long as copy mode stayed open.
    #[test]
    fn scrolling_a_frozen_pane_asks_for_no_history_it_could_not_show() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event_at("run_1", b"", 4096, 1));
        dashboard.apply_event(delta_event("run_1", 2, &history_lines(30)));
        render_to_string(&mut dashboard, 100, 30);
        let area = *dashboard.pane_areas.get("a").expect("pane a is rendered");
        assert_eq!(command(&mut dashboard, KeyCode::Char('[')), UiCommand::None);
        assert!(dashboard.copy.is_some(), "the pane is frozen");
        for _ in 0..10 {
            assert_eq!(
                dashboard.mouse(MouseEvent {
                    kind: MouseEventKind::ScrollUp,
                    column: area.x + 2,
                    row: area.y + 2,
                    modifiers: KeyModifiers::NONE,
                }),
                UiCommand::None,
                "a frozen pane cannot show anything a page-back would fetch"
            );
        }
    }

    /// Enough lines that they would unmistakably reach scrollback if they were spliced in —
    /// a single line would be overwritten by the seed's repaint and prove nothing.
    #[test]
    fn history_from_a_restarted_run_is_discarded_rather_than_spliced_in() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event_at("run_1", b"", 4096, 2));
        dashboard.apply_event(delta_event("run_1", 2, &history_lines(60)));
        let before = dashboard.screens["run_1"].screen().contents();
        let ghosts: Vec<u8> = (1..=40)
            .flat_map(|index| format!("ghost {index}\r\n").into_bytes())
            .collect();
        // Epoch 1 is the previous incarnation of this pane; the replica is showing epoch 2.
        let answer = history_response(&dashboard, "run_1", 1, &ghosts, true);
        dashboard.apply_pane_history_response(answer);
        assert_eq!(
            dashboard.screens["run_1"].screen().contents(),
            before,
            "a cursor from before a restart names a position in a stream that no longer exists"
        );
        for _ in 0..500 {
            dashboard.scroll_pane("run_1", 3);
        }
        let scrolled = render_to_string(&mut dashboard, 100, 30);
        assert!(
            !scrolled.contains("ghost 2"),
            "bytes from a dead stream must not be spliced into this one: {scrolled}"
        );
    }

    #[test]
    fn the_prefix_then_bracket_enters_copy_mode_and_escape_leaves_it() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event("run_1", b"hello world\r\n"));
        assert!(!dashboard.copy_mode());
        dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        dashboard.key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
        assert!(dashboard.copy_mode(), "Ctrl+B [ must enter copy mode");
        dashboard.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!dashboard.copy_mode(), "Esc always leaves copy mode");
    }

    /// Drawing and key routing were two hardcoded lists of the same eight overlays, written in
    /// two different orders. The launch form was drawn beneath help and rename but routed after
    /// them; the board was drawn over the Git overlay but routed before it. Neither disagreement
    /// was reachable by using Dock — at most one overlay is open at a time — so nothing but a
    /// test could catch it, and no test could be written while the two orders lived in two `if`
    /// chains with nothing in common to assert about.
    ///
    /// Opening all eight at once is the only way to make the orders observable, which is why
    /// this test does something a user never can.
    #[test]
    fn every_open_overlay_takes_keys_in_the_same_order_it_is_drawn() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event("run_1", b"hello world\r\n"));
        // Copy mode goes first because it is entered through the keymap, and every other overlay
        // is ahead of the keymap: with one of them open the prefix would never arrive.
        dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        dashboard.key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
        assert!(dashboard.copy_mode(), "copy mode is the eighth surface");
        dashboard.help_open = true;
        dashboard.rename_form = Some((RenameTarget::Pane, "ledger".into()));
        dashboard.open_launch();
        dashboard.picker = Some((PickerPurpose::Workspace, Picker::new(Vec::new())));
        dashboard.set_review_inbox(vec![(handoff("dock_01J9", "DOCK-7"), None)]);
        dashboard.set_board_tasks(
            vec![board_task(1, "do the thing", "backlog")],
            crate::board::tasks_dir("", "workspace_1").expect("a workspace board"),
        );
        dashboard.set_git(git_facts(), "diff --git a/x b/x".into());

        // What `render` walks: every open overlay, in `OVERLAY_ORDER`, later ones over earlier.
        assert_eq!(
            dashboard.open_overlays().collect::<Vec<_>>(),
            OVERLAY_ORDER.to_vec(),
            "with all eight open the draw sequence is OVERLAY_ORDER entire"
        );

        // What `key` walks: the first open overlay takes the key. Measured rather than asserted
        // against the constant directly — each Esc is answered by whichever surface actually
        // holds the keyboard, so this is the routing order as a user would experience it.
        let mut routed = Vec::new();
        loop {
            let Some(kind) = dashboard.open_overlays().next() else {
                break;
            };
            routed.push(kind);
            dashboard.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
            assert!(
                !dashboard.overlay_is_open(kind),
                "Esc reached {kind:?}, so {kind:?} must have closed"
            );
        }
        assert_eq!(
            routed,
            OVERLAY_ORDER.to_vec(),
            "the order keys are routed in must be the order overlays are drawn in"
        );
    }

    /// The defect copy mode was named for. `apply_event` keeps feeding every pushed
    /// `PaneDelta` into the parser the pane renders from, so a pane still producing output
    /// scrolled the highlighted text out from under the selection while it was being made:
    /// the mode called itself frozen, the doc comments said "frozen", and the pane moved.
    ///
    /// The fix is a clone of the screen rather than a buffer of the deltas. `vt100` is a
    /// stateful machine, so a chunk dropped when a buffer filled would desynchronise
    /// escape-sequence parsing rather than lose whole lines, and the user would leave copy
    /// mode into a corrupted screen — which is why the last two assertions here check that
    /// the live pane comes back intact and current, with nothing to flush.
    #[test]
    fn copy_mode_holds_the_pane_still_while_output_keeps_arriving_and_releases_it_on_exit() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event("run_1", b"frozen text\r\n"));
        let opened = render_terminal(&mut dashboard, 100, 30);
        let area = *dashboard.pane_areas.get("a").expect("pane a is rendered");
        let body = |terminal: &Terminal<TestBackend>| {
            (area.y + 1..area.bottom() - 1)
                .map(|row| row_text(terminal, area, row))
                .collect::<Vec<_>>()
        };
        assert!(
            body(&opened).concat().contains("frozen text"),
            "{:?}",
            body(&opened)
        );

        dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        dashboard.key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
        dashboard.key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
        dashboard.key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
        for _ in 0..10 {
            dashboard.key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
        }
        let frozen = body(&render_terminal(&mut dashboard, 100, 30));

        // Far more than the pane's 24 rows, so nothing that was on screen at the freeze can
        // still be on a live screen afterwards.
        let mut written = Vec::new();
        for line in 1..=60 {
            written.extend_from_slice(format!("live line {line}\r\n").as_bytes());
        }
        dashboard.apply_event(Event::PaneDelta {
            run_id: "run_1".into(),
            revision: 2,
            bytes: STANDARD.encode(&written),
        });

        let still = body(&render_terminal(&mut dashboard, 100, 30));
        assert_eq!(
            still, frozen,
            "a frozen pane must paint the same rows it painted before the delta arrived"
        );

        dashboard.key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert_eq!(
            dashboard.last_copied.as_deref(),
            Some("frozen text"),
            "the yank must return the text the selection was placed on, not whatever \
             output moved underneath it"
        );

        // The live parser never stopped, so there is nothing to flush: dropping the clone is
        // the whole exit path and the pane is already current.
        assert!(!dashboard.copy_mode(), "yanking leaves copy mode");
        let live = body(&render_terminal(&mut dashboard, 100, 30)).concat();
        for line in 40..=60 {
            assert!(
                live.contains(&format!("live line {line}")),
                "the live pane must come back current and uncorrupted, missing \
                 {line}: {live:?}"
            );
        }
        assert!(
            !live.contains("frozen text"),
            "the frozen rows scrolled into history while the mode was open: {live:?}"
        );
    }

    /// The clone carries the scrollback with it (`vt100::Screen` owns both its grids), which
    /// is the whole reason a freeze can be a copy rather than a pause: history stays reachable
    /// from inside the mode, and reaching it cannot be undone by output still arriving.
    #[test]
    fn scrolling_inside_a_frozen_pane_reaches_the_history_the_clone_carried_with_it() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event("run_1", b""));
        // Through a delta rather than the attach snapshot: a snapshot carries the screen only,
        // so a replica seeded from one has no history to be cloned along with it.
        let mut written = Vec::new();
        for line in 1..=60 {
            written.extend_from_slice(format!("line {line}\r\n").as_bytes());
        }
        dashboard.apply_event(Event::PaneDelta {
            run_id: "run_1".into(),
            revision: 2,
            bytes: STANDARD.encode(&written),
        });
        render_to_string(&mut dashboard, 100, 30);

        dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        dashboard.key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
        dashboard.key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
        // `g` parks the cursor on the top row, so every one of these walks a row into history.
        for _ in 0..20 {
            dashboard.key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
        }
        assert_eq!(
            dashboard
                .copy
                .as_ref()
                .expect("still frozen")
                .frozen
                .scroll_offset(),
            20,
            "`k` past the top row must move the frozen viewport, which is the one on screen"
        );
        assert_eq!(
            dashboard.screens["run_1"].scroll_offset(),
            0,
            "the live replica keeps following its own output while the mode reads history"
        );

        dashboard.apply_event(Event::PaneDelta {
            run_id: "run_1".into(),
            revision: 3,
            bytes: STANDARD.encode(b"line 61\r\n"),
        });
        let text = render_to_string(&mut dashboard, 100, 30);
        assert!(
            text.contains("line 18"),
            "twenty rows back from the tail of sixty is where the viewport should be: {text:?}"
        );
        assert!(
            !text.contains("line 61"),
            "output arriving behind a scrolled-back freeze must not move it: {text:?}"
        );
    }

    /// A re-attach is the daemon saying the pane's *geometry* changed, so every coordinate the
    /// session holds was chosen against a grid that no longer exists. Painting the old
    /// snapshot into the new rect would show rows that are nowhere, and keeping the
    /// coordinates would yank text nobody pointed at, so the mode ends and says so.
    #[test]
    fn a_pane_re_seeded_while_it_is_frozen_ends_copy_mode_rather_than_selecting_a_grid_that_is_gone()
     {
        for reseed in [
            Event::PaneAttached {
                run_id: "run_1".into(),
                revision: 2,
                rows: 12,
                cols: 20,
                scrollback_rows: 2000,
                history_from: 0,
                epoch: 1,
                screen: String::new(),
            },
            // The other re-seed trigger: a revision gap means this client missed bytes, so
            // the replica is dropped and rebuilt from a fresh snapshot.
            Event::PaneDelta {
                run_id: "run_1".into(),
                revision: 9,
                bytes: STANDARD.encode(b"unreachable"),
            },
        ] {
            let mut dashboard = bound_dashboard();
            dashboard.apply_event(attach_event("run_1", b"before the resize\r\n"));
            render_to_string(&mut dashboard, 100, 30);
            dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
            dashboard.key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
            dashboard.key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
            assert!(dashboard.copy_mode());

            dashboard.apply_event(reseed);
            assert!(
                !dashboard.copy_mode(),
                "a selection over a grid that has been replaced cannot be honoured"
            );
            let notice = dashboard.error.clone().unwrap_or_default();
            assert!(
                notice.starts_with("copy mode ended"),
                "a mode that swallows every key must say when it stops, got {notice:?}"
            );
        }
    }

    /// `detach_screens` drops every replica so a re-established stream can re-attach them. The
    /// frozen clone would happily outlive its replica, but a selection over rows the user is
    /// about to stop being shown is a selection of nothing they can see.
    #[test]
    fn re_establishing_the_event_stream_ends_a_frozen_selection_with_the_screens_it_came_from() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event("run_1", b"before the reconnect\r\n"));
        render_to_string(&mut dashboard, 100, 30);
        dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        dashboard.key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
        assert!(dashboard.copy_mode());
        dashboard.detach_screens();
        assert!(!dashboard.copy_mode(), "the pane it froze is gone");
        assert!(
            dashboard
                .error
                .as_deref()
                .is_some_and(|notice| notice.starts_with("copy mode ended")),
            "{:?}",
            dashboard.error
        );
    }

    /// The snapshot holds no reference back to the parser it was cloned from, so a run that
    /// dies under an open selection leaves the mode entirely usable: the pane keeps painting
    /// the rows the user froze, and the yank still reaches them. The alternative — blanking
    /// the pane the instant the process went — would destroy the last thing it printed, which
    /// is exactly what somebody in copy mode over a dying run is trying to keep.
    #[test]
    fn a_run_that_dies_while_its_pane_is_frozen_still_paints_yanks_and_exits() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event("run_1", b"last words\r\n"));
        render_to_string(&mut dashboard, 100, 30);
        dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        dashboard.key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
        dashboard.key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
        dashboard.key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
        for _ in 0..9 {
            dashboard.key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
        }

        dashboard.layout.workspaces[0]
            .panes
            .get_mut("a")
            .unwrap()
            .runtime = PaneRuntime::Exited;
        dashboard.set_runs(vec![]);
        let text = render_to_string(&mut dashboard, 100, 30);
        assert!(
            text.contains("last words"),
            "the frozen rows outlive the run that printed them: {text:?}"
        );

        dashboard.key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert_eq!(dashboard.last_copied.as_deref(), Some("last words"));
        assert!(!dashboard.copy_mode(), "and the mode still exits");
    }

    /// A pane entering the alternate screen is just more bytes into a parser copy mode is no
    /// longer reading from, so the freeze holds through it — and, because the parser was never
    /// interrupted, the full-screen program is fully painted the moment the mode ends. This is
    /// the case a buffer-and-flush design handles worst: `\x1b[?1049h` is precisely the sort of
    /// escape sequence a dropped chunk would cut in half.
    #[test]
    fn a_pane_entering_the_alternate_screen_while_frozen_neither_moves_it_nor_corrupts_it() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event("run_1", b"ordinary output\r\n"));
        render_to_string(&mut dashboard, 100, 30);
        dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        dashboard.key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
        dashboard.key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
        dashboard.key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
        for _ in 0..14 {
            dashboard.key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
        }

        dashboard.apply_event(Event::PaneDelta {
            run_id: "run_1".into(),
            revision: 2,
            bytes: STANDARD.encode(b"\x1b[?1049h\x1b[2J\x1b[HFULL SCREEN PROGRAM"),
        });
        let frozen = render_to_string(&mut dashboard, 100, 30);
        assert!(frozen.contains("ordinary output"), "{frozen:?}");
        assert!(
            !frozen.contains("FULL SCREEN PROGRAM"),
            "the alternate screen must not reach a frozen pane: {frozen:?}"
        );

        dashboard.key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert_eq!(dashboard.last_copied.as_deref(), Some("ordinary output"));
        let live = render_to_string(&mut dashboard, 100, 30);
        assert!(
            live.contains("FULL SCREEN PROGRAM"),
            "the live parser consumed the switch as it arrived, so there is nothing to \
             replay on exit: {live:?}"
        );
    }

    #[test]
    fn copy_mode_keys_never_reach_the_pane() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event("run_1", b"hello world\r\n"));
        dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        dashboard.key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
        // Ctrl+C, Ctrl+D and Enter are the three that would actually do something to the
        // shell — interrupt it, close its stdin, run whatever is on the line. `y` is last
        // because yanking deliberately leaves the mode, and every key before it must find
        // copy mode still in charge for the loop to be proving containment at all.
        for key in [
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
        ] {
            assert!(
                dashboard.copy_mode(),
                "copy mode must still be active to be the thing swallowing {key:?}"
            );
            let outcome = dashboard.key(key);
            assert!(
                !matches!(outcome, UiCommand::PaneInput(_)),
                "{key:?} must not be forwarded to the PTY while in copy mode"
            );
        }
        assert!(!dashboard.copy_mode(), "the trailing y yanked and left");
    }

    #[test]
    fn a_bare_yank_copies_the_cursors_line_and_names_it() {
        let mut dashboard = bound_dashboard();
        // No trailing newline, so the live cursor — and therefore copy mode's — sits on the
        // row that actually holds the text.
        dashboard.apply_event(attach_event("run_1", b"https://example.test/path"));
        dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        dashboard.key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
        dashboard.key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(!dashboard.copy_mode(), "yanking leaves copy mode");
        let notice = dashboard.error.clone().unwrap_or_default();
        // 25 characters is the URL with the row's trailing blanks trimmed off, which is the
        // proof that the line — not the whole padded grid row — went to the clipboard.
        assert!(
            notice.contains("copied line 1 (25 characters)"),
            "a bare yank must copy the cursor's line and say so, got {notice:?}"
        );
    }

    #[test]
    fn escape_cancels_the_search_prompt_first_and_copy_mode_second() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event("run_1", b"needle in here"));
        dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        dashboard.key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
        dashboard.key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        dashboard.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(dashboard.copy_status().as_deref(), Some("COPY /n"));
        dashboard.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            dashboard.copy_mode(),
            "the first Esc cancels the prompt, not the mode"
        );
        assert_eq!(
            dashboard.copy_status().as_deref(),
            Some("COPY MOVE 1,15"),
            "the prompt is gone and the cursor is where it was"
        );
        dashboard.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!dashboard.copy_mode(), "the second Esc leaves copy mode");
    }

    #[test]
    fn control_modified_letters_are_not_copy_mode_bindings() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event("run_1", b"hello world"));
        dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        dashboard.key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
        let before = dashboard.copy_status();
        for key in ['h', 'j', 'v', 'g'] {
            dashboard.key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::CONTROL));
        }
        assert_eq!(
            dashboard.copy_status(),
            before,
            "Ctrl+letter is somebody reaching past copy mode, not a motion or a verb"
        );
        // Shift is the exception: crossterm reports uppercase G with it set, so requiring a
        // literally empty modifier set would take `G` and `N` away.
        dashboard.key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT));
        assert_eq!(
            dashboard.copy_status().as_deref(),
            Some(format!("COPY MOVE {},1", PANE_ROWS).as_str())
        );
    }

    #[test]
    fn copy_mode_publishes_a_status_line_for_the_footer_to_render() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event("run_1", b"hello world"));
        assert_eq!(
            dashboard.copy_status(),
            None,
            "there is no indicator when the mode is off"
        );
        dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        dashboard.key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
        let status = dashboard.copy_status().unwrap_or_default();
        assert!(
            status.contains("COPY"),
            "a modal mode must have something to show, got {status:?}"
        );
        dashboard.key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
        assert!(
            dashboard
                .copy_status()
                .unwrap_or_default()
                .contains("SELECT"),
            "starting a selection must be visible too"
        );
    }

    #[test]
    fn yanking_a_selection_reports_which_clipboard_route_was_used() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event("run_1", b"copy me\r\n"));
        dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        dashboard.key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
        dashboard.key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
        dashboard.key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
        dashboard.key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(!dashboard.copy_mode(), "yanking leaves copy mode");
        let notice = dashboard.error.clone().unwrap_or_default();
        assert!(
            notice.contains("copied") || notice.contains("clipboard"),
            "the yank must say what happened, got {notice:?}"
        );
    }

    // CONTROLLER RULING C2 again: the brief's version of both tests below used
    // `dashboard()`, whose pane "a" has `run_id: None`. Copy mode is refused on an unbound
    // pane, so neither test could have exercised anything. `bound_dashboard()` binds pane
    // "a" to "run_1", which is also the run the attach event seeds.
    #[test]
    fn copy_mode_is_visibly_signalled_in_the_pane_and_footer() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event("run_1", b"visible text\r\n"));
        let before = render_terminal(&mut dashboard, 100, 30);
        let area = *dashboard.pane_areas.get("a").expect("pane a is rendered");
        assert!(
            !row_text(&before, area, area.y).contains("COPY"),
            "the title must not claim copy mode before it is entered"
        );

        dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        dashboard.key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
        let terminal = render_terminal(&mut dashboard, 100, 30);

        // REVIEW FINDING 2: asserting `COPY` against the whole frame was passed entirely by
        // the footer's own indicator, so the title prefix had no coverage at all. The pane
        // title lives on the pane's top border row, so scope the search to exactly that.
        let title = row_text(&terminal, area, area.y);
        assert!(
            title.contains("COPY"),
            "the pane title must say it is in copy mode, got {title:?}"
        );

        let rendered = rendered(&terminal);
        // The brief asked for `contains('y')`, which every ordinary word in the UI already
        // satisfies; only a distinctive slice of the copy footer proves it actually changed.
        assert!(
            rendered.contains("y yank"),
            "the footer must publish the yank binding, got {rendered:?}"
        );
        assert!(
            !rendered.contains("keys go to the focused pane"),
            "the live-pane hint is wrong while every key is being swallowed"
        );
    }

    // REVIEW FINDING 1: `render_to_string` keeps only `cell.symbol()`, so deleting the whole
    // selection overlay left every test green. These two read the buffer's styles instead,
    // which is the only way this task's headline deliverable is guarded at all.
    #[test]
    fn the_selection_and_copy_cursor_are_painted_only_while_copy_mode_is_active() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event("run_1", b"hello world\r\n"));
        let selection = dashboard.theme.selection;
        let accent = dashboard.theme.accent;
        let surface = dashboard.theme.surface;

        let quiet = render_terminal(&mut dashboard, 100, 30);
        let area = *dashboard.pane_areas.get("a").expect("pane a is rendered");
        assert!(
            cells_with_background(&quiet, area, selection).is_empty(),
            "a pane that is not in copy mode must carry no selection highlight"
        );

        dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        dashboard.key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
        dashboard.key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
        dashboard.key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
        for _ in 0..3 {
            dashboard.key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
        }
        assert_eq!(copy_selection(&dashboard), Some(((0, 0), (0, 3))));

        let terminal = render_terminal(&mut dashboard, 100, 30);
        // The cursor cell is the fourth of the run and is painted as the cursor rather than
        // as selection, so the highlight is exactly the three cells behind it — and nothing
        // anywhere else in the pane.
        assert_eq!(
            cells_with_background(&terminal, area, selection),
            vec![(0, 0), (0, 1), (0, 2)],
            "the highlight must cover the selection and only the selection"
        );
        let buffer = terminal.backend().buffer();
        let cursor = &buffer[(area.x + 1 + 3, area.y + 1)];
        assert_eq!(cursor.bg, accent, "the copy cursor must be findable");
        assert_eq!(cursor.fg, surface, "and legible against its own block");
        assert_ne!(
            buffer[(area.x + 1 + 4, area.y + 1)].bg,
            selection,
            "the cell past the cursor is outside the selection"
        );

        dashboard.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let after = render_terminal(&mut dashboard, 100, 30);
        assert!(
            cells_with_background(&after, area, selection).is_empty(),
            "leaving copy mode must take the highlight with it"
        );
    }

    /// WHOLE-BRANCH REVIEW C1. The overlay and `VtTerminal::selection_text` are two
    /// independent answers to "which cells are selected": the overlay walks `first..=last`
    /// itself, the yank asks `vt100::contents_between`. Each half was internally consistent
    /// and the two disagreed by exactly one cell on *every* selection, because
    /// `contents_between` is column-exclusive while the highlight is inclusive. Every
    /// dragged path or URL lost its last character, and a single-cell selection copied
    /// nothing while the footer still said "copied 0 characters ... via OSC 52". Nine
    /// reviews missed it because no test ever compared the two counts against each other.
    /// This is that comparison, and it is why the end column is now inclusive on both sides.
    #[test]
    fn a_mid_row_selection_yanks_exactly_as_many_characters_as_it_highlights() {
        const ROW: &str = "ABCDEFGHIJKLMNOP";
        for extra in 0..5u16 {
            let mut dashboard = bound_dashboard();
            dashboard.apply_event(attach_event("run_1", format!("{ROW}\r\n").as_bytes()));
            let selection = dashboard.theme.selection;
            let accent = dashboard.theme.accent;

            dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
            dashboard.key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
            dashboard.key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
            dashboard.key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
            for _ in 0..extra {
                dashboard.key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
            }

            let terminal = render_terminal(&mut dashboard, 100, 30);
            let area = *dashboard.pane_areas.get("a").expect("pane a is rendered");
            // The overlay paints the run with `theme.selection` and then repaints the
            // cursor's own cell with `theme.accent`, so the highlighted run the user sees is
            // the selection cells plus that one.
            let cursor = &terminal.backend().buffer()[(area.x + 1 + extra, area.y + 1)];
            assert_eq!(cursor.bg, accent, "the copy cursor sits inside the run");
            let highlighted = cells_with_background(&terminal, area, selection).len() + 1;
            assert_eq!(
                highlighted,
                usize::from(extra) + 1,
                "{extra} rightward moves must highlight {} cells",
                extra + 1
            );

            // Read before the yank: `y` leaves copy mode, taking the session with it.
            let (from, to) = copy_selection(&dashboard).expect("the selection is anchored");
            dashboard.key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
            let notice = dashboard.error.clone().expect("a yank reports itself");
            assert!(
                notice.starts_with(&format!("copied {highlighted} characters")),
                "{highlighted} highlighted cells must be reported as {highlighted} \
                 characters, got {notice:?}"
            );
            let yanked = dashboard
                .screens
                .get("run_1")
                .expect("the pane has a screen")
                .selection_text(from, to);
            assert_eq!(
                yanked,
                ROW[..highlighted],
                "the clipboard must hold exactly the highlighted cells"
            );
        }
    }

    #[test]
    fn the_ptys_own_cursor_yields_to_the_copy_cursor_and_returns_afterwards() {
        let mut dashboard = bound_dashboard();
        // Ends on a newline, so the PTY cursor sits on a blank cell and tui-term draws its
        // block symbol there — which is what `draws_cursor` looks for.
        dashboard.apply_event(attach_event("run_1", b"hello world\r\n"));
        let live = render_terminal(&mut dashboard, 100, 30);
        let area = *dashboard.pane_areas.get("a").expect("pane a is rendered");
        assert!(
            draws_cursor(&live, area),
            "the focused live pane shows where typing lands"
        );

        dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        dashboard.key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
        let copying = render_terminal(&mut dashboard, 100, 30);
        assert!(
            !draws_cursor(&copying, area),
            "two cursor blocks make it ambiguous which one the keys are moving"
        );

        dashboard.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let back = render_terminal(&mut dashboard, 100, 30);
        assert!(
            draws_cursor(&back, area),
            "leaving copy mode gives the pane its own cursor back"
        );
    }

    /// REWRITTEN. This test used to pin the opposite contract — "releasing a drag must not
    /// write to the clipboard" — on the reasoning that a stray drag could otherwise overwrite
    /// what the user copied earlier. That reasoning is sound and it is now enforced somewhere
    /// better (a gesture that selected nothing copies nothing; see the test below), while the
    /// contract it protected cost Dock the behaviour every terminal a user arrives from
    /// already has. iTerm2, Ghostty, WezTerm and GNOME Terminal all copy on release; a
    /// selection that then needs `y` pressed reads as a selection that did not work.
    #[test]
    fn releasing_a_drag_copies_the_selection_and_leaves_it_highlighted() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event("run_1", b"drag over me\r\n"));
        render_to_string(&mut dashboard, 100, 30);
        let area = *dashboard.pane_areas.get("a").expect("pane a is rendered");
        dashboard.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x + 2,
            row: area.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        assert!(
            !dashboard.copy_mode(),
            "a bare click focuses a pane; it must not trap the keyboard in a mode"
        );
        dashboard.mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: area.x + 8,
            row: area.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        assert!(
            dashboard.copy_mode(),
            "a drag inside a pane enters copy mode"
        );
        assert_eq!(
            copy_selection(&dashboard).expect("the drag anchored a selection"),
            ((0, 1), (0, 7)),
            "the anchor is the press cell and the cursor follows the pointer"
        );
        dashboard.mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: area.x + 8,
            row: area.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        assert!(
            dashboard.copy_mode(),
            "releasing finalises the selection but stays in copy mode"
        );
        // Columns 1..=7 of "drag over me" inclusive at both ends, which is what the highlight
        // covered — the same inclusive run `a_mid_row_selection_yanks_exactly_as_many_        // characters_as_it_highlights` pins.
        assert_eq!(
            dashboard.last_copied.as_deref(),
            Some("rag ove"),
            "release copies exactly what was highlighted"
        );
        let notice = dashboard.error.clone().unwrap_or_default();
        assert!(
            notice.contains("copied 7 characters"),
            "the copy names itself, got {notice:?}"
        );
        assert!(
            notice.contains("acknowledge"),
            "an OSC 52 copy must not claim a clipboard it cannot check, got {notice:?}"
        );
        let rendered = render_to_string(&mut dashboard, 100, 30);
        assert!(
            rendered.contains("COPY"),
            "a pointer selection puts the pane in the same visible mode a keyboard one does"
        );
    }

    /// The guarantee the old contract was really protecting, kept: a gesture that selected
    /// nothing must not touch the clipboard. Without this, every click that focused a pane
    /// would re-copy whatever selection happened to still be standing and overwrite anything
    /// the user had put on their clipboard from elsewhere in between.
    #[test]
    fn a_click_that_never_dragged_leaves_the_clipboard_alone() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event("run_1", b"drag over me\r\n"));
        render_to_string(&mut dashboard, 100, 30);
        let area = *dashboard.pane_areas.get("a").expect("pane a is rendered");
        let press = |column: u16| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row: area.y + 1,
            modifiers: KeyModifiers::NONE,
        };
        let release = |column: u16| MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column,
            row: area.y + 1,
            modifiers: KeyModifiers::NONE,
        };
        dashboard.mouse(press(area.x + 2));
        dashboard.mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: area.x + 8,
            row: area.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        dashboard.mouse(release(area.x + 8));
        assert_eq!(dashboard.last_copied.as_deref(), Some("rag ove"));

        dashboard.error = None;
        // A press on a far-away cell, so it cannot be read as a second click of the first.
        dashboard.mouse(press(area.x + 20));
        dashboard.mouse(release(area.x + 20));
        assert_eq!(
            dashboard.last_copied.as_deref(),
            Some("rag ove"),
            "a click with no drag must leave the earlier copy standing"
        );
        assert!(
            dashboard.error.is_none(),
            "and it must not report a copy that did not happen"
        );
    }

    /// Double click selects a word, triple click a line — the behaviour of every terminal, and
    /// the reason `is_word_character` treats a path as one thing rather than five.
    #[test]
    fn a_double_click_selects_a_word_and_a_triple_click_the_whole_line() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event("run_1", b"see src/main.rs:12 now"));
        render_to_string(&mut dashboard, 100, 30);
        let area = *dashboard.pane_areas.get("a").expect("pane a is rendered");
        // Column 6 of the row, which is inside "src/main.rs:12".
        let press = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x + 1 + 6,
            row: area.y + 1,
            modifiers: KeyModifiers::NONE,
        };
        dashboard.mouse(press);
        dashboard.mouse(press);
        assert_eq!(
            copy_selection(&dashboard).expect("the double click selected a word"),
            ((0, 4), (0, 17)),
            "a path with a line number is one word, not five"
        );
        dashboard.mouse(press);
        assert_eq!(
            copy_selection(&dashboard).expect("the triple click selected a line"),
            ((0, 0), (0, 21)),
            "the line stops at its last character, not at the padded width of the grid"
        );
        dashboard.mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: press.column,
            row: press.row,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(
            dashboard.last_copied.as_deref(),
            Some("see src/main.rs:12 now"),
            "releasing a multi-click copies it like any other selection"
        );
    }

    #[test]
    fn a_middle_or_right_click_pastes_the_last_copied_text_into_the_focused_pane() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event("run_1", b"paste me\r\n"));
        render_to_string(&mut dashboard, 100, 30);
        let area = *dashboard.pane_areas.get("a").expect("pane a is rendered");
        let inside = |kind| MouseEvent {
            kind,
            column: area.x + 2,
            row: area.y + 1,
            modifiers: KeyModifiers::NONE,
        };
        // Nothing copied yet: the click says so rather than typing something arbitrary.
        assert_eq!(
            dashboard.mouse(inside(MouseEventKind::Down(MouseButton::Middle))),
            UiCommand::None
        );
        assert!(
            dashboard
                .error
                .as_deref()
                .is_some_and(|notice| notice.contains("nothing copied yet")),
            "got {:?}",
            dashboard.error
        );

        dashboard.mouse(inside(MouseEventKind::Down(MouseButton::Left)));
        dashboard.mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: area.x + 5,
            row: area.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        dashboard.mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: area.x + 5,
            row: area.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(dashboard.last_copied.as_deref(), Some("aste"));

        for button in [MouseButton::Middle, MouseButton::Right] {
            assert_eq!(
                dashboard.mouse(inside(MouseEventKind::Down(button))),
                UiCommand::PaneInput(b"aste".to_vec()),
                "{button:?} pastes what was last copied, through the paste encoder"
            );
        }
        // A press outside the focused pane's body is not a paste into it: input is
        // destructive, and a click that landed elsewhere must never be typed here.
        assert_eq!(
            dashboard.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Middle),
                column: 1,
                row: 1,
                modifiers: KeyModifiers::NONE,
            }),
            UiCommand::None
        );
    }

    /// A divider drag used to send a `Resize` per motion event, and each one cost a blocking
    /// request plus `refresh`'s two more, so the divider crawled behind the pointer. The local
    /// layout still moves on every event; only the daemon call waits for the release.
    #[test]
    fn a_divider_drag_asks_the_daemon_to_resize_once_when_the_button_comes_up() {
        let mut dashboard = bound_dashboard();
        render_to_string(&mut dashboard, 100, 30);
        let divider = dashboard
            .dividers
            .first()
            .cloned()
            .expect("a split divider");
        dashboard.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: divider.area.x,
            row: divider.area.y,
            modifiers: KeyModifiers::NONE,
        });
        for offset in 1..=6 {
            assert_eq!(
                dashboard.mouse(MouseEvent {
                    kind: MouseEventKind::Drag(MouseButton::Left),
                    column: divider.area.x + offset,
                    row: divider.area.y,
                    modifiers: KeyModifiers::NONE,
                }),
                UiCommand::None,
                "a divider drag must not put a blocking round trip on every motion event"
            );
        }
        let released = dashboard.mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: divider.area.x + 6,
            row: divider.area.y,
            modifiers: KeyModifiers::NONE,
        });
        let UiCommand::Request(request) = released else {
            panic!("releasing a divider must ask the daemon for the ratio it finished on");
        };
        let Request::Workspace(WorkspaceRequest::Resize { ratio_milli, .. }) = *request else {
            panic!("expected a resize");
        };
        assert!(
            ratio_milli > 500,
            "the ratio sent is where the pointer ended up, got {ratio_milli}"
        );
        assert!(
            dashboard.pending_divider_resize.is_none(),
            "the held ratio is consumed by the release, not resent on the next one"
        );
    }

    /// Selection endpoints are cells of the visible grid, so a viewport that moves under them
    /// leaves them pointing at different text. Before this, scrolling mid-selection silently
    /// re-aimed the anchor and the yank covered rows the highlight never touched.
    #[test]
    fn scrolling_a_pane_carries_an_anchored_selection_along_with_the_text() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event("run_1", b"seed\r\n"));
        // Through a delta rather than the attach snapshot: a snapshot carries the screen only,
        // so a replica seeded from one has no history for the wheel to reach.
        let mut output = Vec::new();
        for line in 1..=40 {
            output.extend_from_slice(format!("line {line}\r\n").as_bytes());
        }
        dashboard.apply_event(Event::PaneDelta {
            run_id: "run_1".into(),
            revision: 2,
            bytes: STANDARD.encode(&output),
        });
        render_to_string(&mut dashboard, 100, 30);
        let area = *dashboard.pane_areas.get("a").expect("pane a is rendered");
        dashboard.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x + 1,
            row: area.y + 2,
            modifiers: KeyModifiers::NONE,
        });
        dashboard.mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: area.x + 6,
            row: area.y + 2,
            modifiers: KeyModifiers::NONE,
        });
        // Read through the frozen screen, because that is the one the pane is painting and
        // the one the wheel now moves: the live parser is left following its own output.
        let selected = |dashboard: &Dashboard| {
            let mode = dashboard.copy.as_ref().expect("a session");
            let (from, to) = mode.session.selection().expect("an anchored selection");
            mode.frozen.selection_text(from, to)
        };
        let before = selected(&dashboard);
        assert!(!before.trim().is_empty(), "the drag selected something");

        dashboard.mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: area.x + 6,
            row: area.y + 2,
            modifiers: KeyModifiers::NONE,
        });
        assert_ne!(
            dashboard
                .copy
                .as_ref()
                .expect("still frozen")
                .frozen
                .scroll_offset(),
            0,
            "the wheel moved the viewport"
        );
        assert_eq!(
            selected(&dashboard),
            before,
            "the selection must still hold the characters it was placed on"
        );
    }

    #[test]
    fn clicking_the_pane_that_already_has_focus_asks_the_daemon_for_nothing() {
        let mut dashboard = bound_dashboard();
        dashboard.apply_event(attach_event("run_1", b"already here\r\n"));
        render_to_string(&mut dashboard, 100, 30);
        let focused = *dashboard.pane_areas.get("a").expect("pane a is rendered");
        let other = *dashboard.pane_areas.get("b").expect("pane b is rendered");
        assert_eq!(
            dashboard.workspace().expect("a workspace").focused_pane_id,
            "a"
        );
        assert_eq!(
            dashboard.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: focused.x + 2,
                row: focused.y + 1,
                modifiers: KeyModifiers::NONE,
            }),
            UiCommand::None,
            "re-focusing the focused pane cost three blocking round trips and answered nothing"
        );
        // A press on a *different* pane still has something to tell the daemon.
        let moved = dashboard.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: other.x + 2,
            row: other.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        assert!(
            matches!(
                moved,
                UiCommand::Send(ref request)
                    if matches!(**request, Request::Workspace(WorkspaceRequest::Focus { .. }))
            ),
            "got {moved:?}"
        );
    }

    #[test]
    fn a_word_is_the_run_a_terminal_user_means_by_one() {
        // Paths, identifiers, URLs and compiler locations select whole.
        assert_eq!(word_bounds("see src/main.rs:12 now", 6), Some((4, 17)));
        assert_eq!(word_bounds("cd ~/work/dock-2 ok", 5), Some((3, 15)));
        assert_eq!(word_bounds("mail to a@b.test now", 9), Some((8, 15)));
        // A hyphen binds, so `dock-2` above is one word and this is `-` joined to nothing.
        assert_eq!(word_bounds("a -> b", 2), Some((2, 2)));
        // A run of characters that bind to nothing is a word of its own rather than swallowing
        // a neighbour: double-clicking the arrow selects the arrow.
        assert_eq!(word_bounds("a => b", 2), Some((2, 3)));
        // Padding selects nothing: nobody double-clicks blanks on purpose, and copying them
        // would replace a clipboard the user filled deliberately.
        assert_eq!(word_bounds("a  b", 1), None);
        assert_eq!(word_bounds("short", 40), None);
    }

    #[test]
    fn a_line_selection_stops_at_the_last_character_rather_than_the_padded_width() {
        assert_eq!(line_bounds("hello world      "), Some((0, 10)));
        assert_eq!(line_bounds("x"), Some((0, 0)));
        assert_eq!(line_bounds("      "), None, "a blank row selects nothing");
        assert_eq!(line_bounds(""), None);
    }

    #[test]
    fn copy_mode_is_refused_on_a_pane_with_no_run() {
        let mut dashboard = dashboard();
        dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        dashboard.key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
        assert!(!dashboard.copy_mode());
        assert!(
            dashboard.error.is_some(),
            "an impossible command must explain itself rather than doing nothing"
        );
    }

    // ---------------------------------------------------------------------------
    // Render measurement.
    //
    // Not an assertion: `#[ignore]`d so `cargo test` never spends a second on it, and run
    // deliberately with
    //
    //     cargo test --release render_measurement -- --ignored --nocapture
    //
    // when a change is meant to make painting cheaper. The dashboard it paints is shaped like
    // a busy afternoon rather than like a unit test, because every cost this exists to find is
    // a cost that only appears once there are several workspaces, a dozen panes and an agent
    // in each: per-frame deep copies of the layout, per-agent scans across every workspace's
    // panes, and per-pane scans of the run list.
    // ---------------------------------------------------------------------------

    /// Counts every allocation the test binary makes, so a frame can be measured in
    /// allocations as well as in milliseconds — the two costs this render path has are
    /// walking cells and building strings nobody keeps, and only the second shows up here.
    ///
    /// `Relaxed` because the number is a diagnostic, not a synchronisation point; the
    /// measurement is single-threaded and nothing branches on the count.
    struct CountingAllocator;

    static ALLOCATIONS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static ALLOCATED_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    unsafe impl std::alloc::GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
            ALLOCATIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            ALLOCATED_BYTES.fetch_add(layout.size() as u64, std::sync::atomic::Ordering::Relaxed);
            unsafe { std::alloc::System.alloc(layout) }
        }

        unsafe fn dealloc(&self, pointer: *mut u8, layout: std::alloc::Layout) {
            unsafe { std::alloc::System.dealloc(pointer, layout) }
        }

        unsafe fn realloc(
            &self,
            pointer: *mut u8,
            layout: std::alloc::Layout,
            new_size: usize,
        ) -> *mut u8 {
            ALLOCATIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            ALLOCATED_BYTES.fetch_add(new_size as u64, std::sync::atomic::Ordering::Relaxed);
            unsafe { std::alloc::System.realloc(pointer, layout, new_size) }
        }
    }

    #[global_allocator]
    static GLOBAL: CountingAllocator = CountingAllocator;

    /// Output with enough colour and enough width to make every cell of a pane cost what a
    /// real agent's output costs. A blank screen is the one thing a render benchmark must not
    /// measure: `PseudoTerminal` skips cells with no contents, so an empty pane is free and an
    /// empty benchmark says painting is free.
    fn benchmark_pane_content(seed: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        for line in 0..140 {
            bytes.extend_from_slice(
                format!(
                    "\x1b[38;5;{colour}m{line:>4}\x1b[0m \x1b[1mdock\x1b[0m run {seed} \
                     · compiling crate {line} of 240 \x1b[32mok\x1b[0m \
                     · src/dashboard.rs:{line} · {padding}\r\n",
                    colour = (line % 200) + 16,
                    padding = "▁".repeat(60),
                )
                .as_bytes(),
            );
        }
        bytes
    }

    /// A balanced split tree over `pane_ids`, alternating axis by depth the way a person
    /// splitting panes by hand ends up doing.
    fn benchmark_layout_tree(pane_ids: &[String], depth: usize) -> LayoutNode {
        if pane_ids.len() == 1 {
            return LayoutNode::Pane {
                pane_id: pane_ids[0].clone(),
            };
        }
        let middle = pane_ids.len() / 2;
        LayoutNode::Split {
            axis: if depth.is_multiple_of(2) {
                SplitAxis::Vertical
            } else {
                SplitAxis::Horizontal
            },
            ratio_milli: 500,
            first: Box::new(benchmark_layout_tree(&pane_ids[..middle], depth + 1)),
            second: Box::new(benchmark_layout_tree(&pane_ids[middle..], depth + 1)),
        }
    }

    /// `workspaces` workspaces of `panes_each` panes, every pane bound to a run, every run
    /// carrying an agent and a screen with real output on it.
    fn benchmark_dashboard(workspaces: usize, panes_each: usize) -> Dashboard {
        const KINDS: [AgentKind; 4] = [
            AgentKind::Claude,
            AgentKind::Codex,
            AgentKind::Amp,
            AgentKind::Gemini,
        ];
        const STATES: [AgentState; 4] = [
            AgentState::Working,
            AgentState::Blocked,
            AgentState::Idle,
            AgentState::Done,
        ];
        let mut dashboard = Dashboard::default();
        for workspace in 0..workspaces {
            let pane_ids: Vec<String> = (0..panes_each)
                .map(|pane| format!("p{workspace}_{pane}"))
                .collect();
            let mut panes = BTreeMap::new();
            for (index, pane_id) in pane_ids.iter().enumerate() {
                let run_id = format!("run_{workspace}_{index}");
                panes.insert(
                    pane_id.clone(),
                    PaneLayout {
                        pane_id: pane_id.clone(),
                        name: format!("pane {index} of workspace {workspace}"),
                        run_id: Some(run_id.clone()),
                        runtime: PaneRuntime::Running,
                        kind: PaneKind::Terminal,
                    },
                );
                let mut screen = PaneScreen::new(100, 220, 2000);
                screen.feed(&benchmark_pane_content(workspace * panes_each + index));
                dashboard.screens.insert(run_id.clone(), screen);
                dashboard.agents.insert(
                    run_id.clone(),
                    (
                        Some(KINDS[(workspace + index) % KINDS.len()]),
                        STATES[index % STATES.len()],
                    ),
                );
                let mut run = snapshot();
                run.run_id = run_id;
                run.workspace_id = format!("w{workspace}");
                run.pane_id = pane_id.clone();
                run.external_task_ref = format!("TASK-{}", workspace * panes_each + index);
                dashboard.runs.push(run);
            }
            dashboard.layout.workspaces.push(WorkspaceLayout {
                workspace_id: format!("w{workspace}"),
                name: format!("workspace {workspace} · long enough to be ellipsised"),
                focused_pane_id: pane_ids[0].clone(),
                panes,
                root: benchmark_layout_tree(&pane_ids, 0),
            });
        }
        dashboard
    }

    /// Milliseconds and allocations for one frame, repainting the same dashboard at the same
    /// size — which is the shape of an idle dashboard being repainted, and therefore the frame
    /// worth making cheap.
    ///
    /// The time reported is the *fastest* of several rounds rather than the mean. A laptop
    /// running a test suite has other work on it, and noise only ever makes a round slower, so
    /// the minimum is the closest thing to the cost of the frame itself; a mean here moved by
    /// 40% between back-to-back runs of the same binary and hid a real 25% improvement.
    fn measure_frame(
        dashboard: &mut Dashboard,
        width: u16,
        height: u16,
        frames: u32,
    ) -> (f64, u64, u64) {
        const ROUNDS: u32 = 7;
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        for _ in 0..5 {
            terminal.draw(|frame| dashboard.render(frame)).unwrap();
        }
        let mut fastest = f64::MAX;
        let mut allocations = 0;
        let mut bytes = 0;
        for _ in 0..ROUNDS {
            let before = ALLOCATIONS.load(std::sync::atomic::Ordering::Relaxed);
            let before_bytes = ALLOCATED_BYTES.load(std::sync::atomic::Ordering::Relaxed);
            let start = std::time::Instant::now();
            for _ in 0..frames {
                terminal.draw(|frame| dashboard.render(frame)).unwrap();
            }
            let elapsed = start.elapsed().as_secs_f64() * 1000.0 / f64::from(frames);
            fastest = fastest.min(elapsed);
            allocations = (ALLOCATIONS.load(std::sync::atomic::Ordering::Relaxed) - before)
                / u64::from(frames);
            bytes = (ALLOCATED_BYTES.load(std::sync::atomic::Ordering::Relaxed) - before_bytes)
                / u64::from(frames);
        }
        (fastest, allocations, bytes)
    }

    /// What holding a pane's byte log costs on the path every delta takes.
    ///
    /// This is the measurement that chose `PANE_HISTORY_TRIM_SLACK`. Enforcing the budget means
    /// moving the whole log down, and a full log is sixteen megabytes; done on every delta that
    /// arrives past the cap, that lands on the event drain, ahead of render, for a daemon that
    /// pushes every 16 ms. Both policies are reported so the difference is the number, not the
    /// argument.
    #[test]
    #[ignore = "a measurement, not an assertion; cargo test --release measure_what_keeping -- --ignored --nocapture"]
    fn measure_what_keeping_a_panes_byte_log_costs() {
        const DELTA: usize = 4 << 10;
        let delta = vec![b'x'; DELTA];

        // What every delta used to pay: a trim the moment the log is over its budget, which
        // memmoves the entire budget down by the size of the delta.
        let mut fastest_eager = f64::MAX;
        for _ in 0..7 {
            let mut log = vec![b'x'; PANE_HISTORY_BYTES + DELTA];
            let start = std::time::Instant::now();
            log.drain(..DELTA);
            fastest_eager = fastest_eager.min(start.elapsed().as_secs_f64() * 1000.0);
            std::hint::black_box(&log);
        }

        // What a delta pays now: an append, and a share of one copy per slack of output.
        let mut dashboard = Dashboard::default();
        dashboard.history.insert(
            "run_1".into(),
            PaneHistoryCursor {
                epoch: 1,
                from: 0,
                complete: false,
                wrapped: false,
                fruitless: false,
                log: vec![b'x'; PANE_HISTORY_BYTES],
            },
        );
        // Four times the slack, so the amortised copy is included several times over rather
        // than landing on a round boundary.
        let deltas = (PANE_HISTORY_TRIM_SLACK / DELTA) * 4;
        let mut fastest_amortised = f64::MAX;
        for _ in 0..3 {
            let start = std::time::Instant::now();
            for _ in 0..deltas {
                dashboard.retain_history_bytes("run_1", &delta);
            }
            let per_delta = start.elapsed().as_secs_f64() * 1000.0 / deltas as f64;
            fastest_amortised = fastest_amortised.min(per_delta);
        }

        println!();
        println!(
            "a {DELTA}-byte delta into a full {}MiB log",
            PANE_HISTORY_BYTES >> 20
        );
        println!("{:>34}  {:>10}", "policy", "ms/delta");
        println!("{:>34}  {fastest_eager:>10.4}", "trim on every delta");
        println!(
            "{:>34}  {fastest_amortised:>10.4}",
            format!("trim past {}MiB of slack", PANE_HISTORY_TRIM_SLACK >> 20)
        );
    }

    /// What a page-back costs, which is the cost of the wheel notch that asks for it.
    ///
    /// A parser cannot be prepended to, so extending a pane's history means replaying every
    /// byte it holds through a fresh parser. That is the number the 2 MB chunk size is chosen
    /// against: the log grows by a chunk per page-back, and the whole of it is replayed each
    /// time, so the cost of the tenth page-back is the cost of replaying twenty megabytes.
    /// Reported at three log sizes for that reason — the shape of the curve is the point, not
    /// any one row of it.
    #[test]
    #[ignore = "a measurement, not an assertion; cargo test --release measure_what_paging_back -- --ignored --nocapture"]
    fn measure_what_paging_back_through_a_panes_history_costs() {
        println!();
        println!("rebuilding a 40x160 replica from its own byte log");
        println!("{:>12}  {:>10}  {:>10}", "lines", "bytes", "ms");
        for lines in [20_000u32, 40_000, 80_000] {
            let log: Vec<u8> = (0..lines)
                .flat_map(|line| format!("line {line} of a long build log\r\n").into_bytes())
                .collect();
            let mut fastest = f64::MAX;
            for _ in 0..7 {
                let mut screen = PaneScreen::new(40, 160, crate::terminal::PANE_HISTORY_MAX_ROWS);
                let start = std::time::Instant::now();
                screen.feed(&log);
                fastest = fastest.min(start.elapsed().as_secs_f64() * 1000.0);
            }
            println!("{lines:>12}  {:>10}  {fastest:>10.2}", log.len());
        }
    }

    /// What entering copy mode costs, which is what a row of retained scrollback costs.
    ///
    /// Copy mode freezes a pane by cloning the grid *and* the scrollback (`terminal/vt.rs`),
    /// so this one gesture pays for every row `PANE_HISTORY_MAX_ROWS` allows, at whatever the
    /// pane is wide. It is therefore both the latency measurement for the gesture — it is
    /// keyboard-driven, so it is felt — and the price list for the constant: the bytes column
    /// divided by the rows column is what one retained row costs, which is the figure the
    /// constant's doc comment quotes.
    ///
    /// The pre-history depth of 2000 rows is measured alongside the current cap so the
    /// difference is the number rather than the argument. **Run with `--test-threads=1`:** the
    /// byte counter is a process-global, and a benchmark sharing the process with another one
    /// reports the other one's allocations as its own.
    #[test]
    #[ignore = "a measurement, not an assertion; cargo test --release measure_what_freezing -- --ignored --nocapture --test-threads=1"]
    fn measure_what_freezing_a_pane_for_copy_mode_costs() {
        println!();
        println!("cloning a replica's grid and scrollback, which is all entering copy mode does");
        println!(
            "{:>10}  {:>10}  {:>10}  {:>14}  {:>12}",
            "replica", "capacity", "ms/entry", "bytes/entry", "bytes/row"
        );
        for (rows, cols) in [(24u16, 80u16), (40, 160)] {
            // 2000 is the depth a replica held before pane history; 50,000 is the cap this
            // branch first shipped and then withdrew. Both are here so the row that matters —
            // the current constant, in the middle — is read against what it replaced.
            for capacity in [2_000usize, crate::terminal::PANE_HISTORY_MAX_ROWS, 50_000] {
                let mut screen = PaneScreen::new(rows, cols, capacity);
                // Enough lines to fill the scrollback to its capacity and then some, so the
                // clone is of a full replica rather than of a half-empty one.
                let log: Vec<u8> = (0..capacity + usize::from(rows) + 100)
                    .map(|line| format!("line {line} of a long build log\r\n"))
                    .collect::<String>()
                    .into_bytes();
                screen.feed(&log);
                let held = screen.history_rows();
                let mut fastest = f64::MAX;
                let mut bytes = u64::MAX;
                for _ in 0..7 {
                    let before = ALLOCATED_BYTES.load(std::sync::atomic::Ordering::Relaxed);
                    let start = std::time::Instant::now();
                    let frozen = screen.snapshot();
                    fastest = fastest.min(start.elapsed().as_secs_f64() * 1000.0);
                    bytes = bytes
                        .min(ALLOCATED_BYTES.load(std::sync::atomic::Ordering::Relaxed) - before);
                    std::hint::black_box(&frozen);
                }
                println!(
                    "{:>10}  {capacity:>10}  {fastest:>10.2}  {bytes:>14}  {:>12}",
                    format!("{rows}x{cols}"),
                    bytes / held.max(1) as u64
                );
            }
        }
    }

    #[test]
    #[ignore = "a measurement, not an assertion; cargo test --release render_measurement -- --ignored --nocapture"]
    fn render_measurement_of_a_busy_dashboard_at_three_terminal_sizes() {
        let mut dashboard = benchmark_dashboard(4, 12);
        println!();
        println!("4 workspaces × 12 panes, 48 runs, 48 agents");
        println!(
            "{:>10}  {:>10}  {:>12}  {:>12}",
            "size", "ms/frame", "allocs/frame", "bytes/frame"
        );
        for (width, height, frames) in [(80u16, 24u16, 400u32), (200, 50, 200), (400, 100, 100)] {
            let (milliseconds, allocations, bytes) =
                measure_frame(&mut dashboard, width, height, frames);
            println!(
                "{:>10}  {milliseconds:>10.3}  {allocations:>12}  {bytes:>12}",
                format!("{width}x{height}")
            );
        }

        // What the Board pane itself costs, against the identical layout with that pane still a
        // terminal. A board pane is a new render surface that participates in every frame, and
        // "it is only one pane" is exactly the kind of claim that turns out to be a scan of the
        // run list per card. Measured on the same dashboard so the only difference is one kind.
        let mut with_board = benchmark_dashboard(4, 12);
        let board_pane = with_board.layout.workspaces[0]
            .panes
            .values()
            .next()
            .map(|pane| pane.pane_id.clone())
            .expect("a pane to turn into a board");
        let pane = with_board.layout.workspaces[0]
            .panes
            .get_mut(&board_pane)
            .unwrap();
        pane.kind = PaneKind::Board;
        pane.run_id = None;
        with_board.set_board_pane_tasks(
            (1..=24)
                .map(|id| {
                    board_task(
                        id,
                        "a card with a title long enough to need ellipsising",
                        crate::board::STATUSES[(id as usize) % crate::board::STATUSES.len()],
                    )
                })
                .collect(),
            "/repo/real/kanban/tasks".into(),
        );
        println!();
        println!("the same layout with one pane turned into a board (24 cards, 48 agents)");
        println!(
            "{:>10}  {:>10}  {:>12}  {:>12}",
            "size", "ms/frame", "allocs/frame", "bytes/frame"
        );
        for (width, height, frames) in [(80u16, 24u16, 400u32), (200, 50, 200), (400, 100, 100)] {
            let (milliseconds, allocations, bytes) =
                measure_frame(&mut with_board, width, height, frames);
            println!(
                "{:>10}  {milliseconds:>10.3}  {allocations:>12}  {bytes:>12}",
                format!("{width}x{height}")
            );
        }
    }

    /// The same frame, broken into the pieces it is made of, so an optimisation is aimed at a
    /// measured cost rather than at a suspected one.
    ///
    /// Every piece below is timed inside `Terminal::draw`, because a `Frame` cannot be built
    /// any other way; the "empty draw" row at the bottom is what that costs on its own, and
    /// every other row is only meaningful once it is subtracted.
    #[test]
    #[ignore = "a measurement, not an assertion; cargo test --release render_breakdown -- --ignored --nocapture"]
    fn render_breakdown_of_a_busy_dashboard_by_the_work_it_does() {
        let mut dashboard = benchmark_dashboard(4, 12);
        for (width, height) in [(80u16, 24u16), (200, 50), (400, 100)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            for _ in 0..5 {
                terminal.draw(|frame| dashboard.render(frame)).unwrap();
            }
            let frames = 200u32;
            let report = |name: &str, milliseconds: f64, allocations: u64| {
                println!("{name:>34}  {milliseconds:>9.3} ms  {allocations:>8} allocs");
            };
            // Fastest round rather than the mean, for the reason `measure_frame` gives: a
            // loaded laptop only ever makes a round slower, so the mean measures the machine
            // and the minimum measures the code.
            macro_rules! timed {
                ($name:literal, $body:expr) => {{
                    let mut fastest = f64::MAX;
                    let mut allocations = 0;
                    for _ in 0..7 {
                        let before = ALLOCATIONS.load(std::sync::atomic::Ordering::Relaxed);
                        let start = std::time::Instant::now();
                        for _ in 0..frames {
                            std::hint::black_box($body);
                        }
                        fastest =
                            fastest.min(start.elapsed().as_secs_f64() * 1000.0 / f64::from(frames));
                        allocations = (ALLOCATIONS.load(std::sync::atomic::Ordering::Relaxed)
                            - before)
                            / u64::from(frames);
                    }
                    report($name, fastest, allocations);
                }};
            }
            println!();
            println!("  ---- {width}x{height} ----");
            timed!(
                "whole frame",
                terminal.draw(|frame| dashboard.render(frame)).unwrap()
            );
            timed!("workspace().cloned()", dashboard.workspace().cloned());
            timed!("agent_roster()", dashboard.agent_roster());
            timed!("footer_line()", dashboard.footer_line());
            let body_height = height.saturating_sub(5);
            let sidebar = Rect::new(0, 3, 28, body_height);
            timed!(
                "render_sidebar()",
                terminal
                    .draw(|frame| dashboard.render_sidebar(frame, sidebar))
                    .unwrap()
            );
            let panes = Rect::new(28, 3, width - 28, body_height);
            let workspace = dashboard.workspace().cloned().unwrap();
            timed!(
                "render_node() over 12 panes",
                terminal
                    .draw(|frame| {
                        dashboard.render_node(frame, panes, &workspace, &workspace.root);
                    })
                    .unwrap()
            );
            let screens: Vec<&PaneScreen> = workspace
                .panes
                .values()
                .filter_map(|pane| dashboard.screens.get(pane.run_id.as_deref()?))
                .collect();
            timed!(
                "PseudoTerminal alone, 12 panes",
                terminal
                    .draw(|frame| {
                        // Each pane gets a twelfth of the body, so the cell count matches what
                        // the real layout paints; only the chrome around it is gone.
                        let pane_width = panes.width / 4;
                        let pane_height = panes.height / 3;
                        for (index, screen) in screens.iter().enumerate() {
                            let area = Rect::new(
                                panes.x + (index as u16 % 4) * pane_width,
                                panes.y + (index as u16 / 4) * pane_height,
                                pane_width,
                                pane_height,
                            );
                            frame.render_widget(PseudoTerminal::new(screen.screen()), area);
                        }
                    })
                    .unwrap()
            );
            timed!("empty draw", terminal.draw(|_frame| {}).unwrap());
        }
    }
}
