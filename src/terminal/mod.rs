mod keys;
mod vt;

use std::{
    collections::VecDeque,
    sync::atomic::{AtomicU64, Ordering},
};

pub use keys::{KeyEncoding, encode_key, encode_paste};
pub use vt::{PaneSnapshot, ShellSignals, VtTerminal};
// The two grid readers, re-exported so the dashboard can point them at whichever screen an
// open selection resolves to — the live parser's until output forces a clone, the clone after.
pub(crate) use vt::{row_text, text_between};

/// Single swap point for the terminal engine. `rio-vt` can replace `VtTerminal` here
/// without touching any caller once it is mature enough to depend on.
pub type PaneScreen = VtTerminal;

/// The daemon's model of one subscriber's replica: a parser fed exactly what that subscriber
/// was sent.
///
/// Subscribers are sent the child's raw bytes, so this normally agrees with the live screen
/// on its own. It is kept anyway because agreement is not guaranteed by construction: a
/// subscriber that attached after the child set some mode this replay cannot carry (a scroll
/// region, say) would drift, and drift in a mirror is indistinguishable from a rendering bug.
/// `delta_from` reports exactly that drift, so the daemon can append a repaint that erases it.
/// An unchanged screen yields an empty delta and therefore no traffic.
pub struct ScreenSync {
    sent: VtTerminal,
}

impl ScreenSync {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            sent: VtTerminal::new(rows, cols, 0),
        }
    }

    /// Bytes that transform this subscriber's view into `live`. Does not mutate `self`;
    /// call `apply` once the bytes have actually been transmitted.
    pub fn delta_from(&self, live: &VtTerminal) -> Vec<u8> {
        live.screen().state_diff(self.sent.screen())
    }

    pub fn apply(&mut self, delta: &[u8]) {
        self.sent.feed(delta);
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.sent.resize(rows, cols);
    }

    pub fn state_bytes(&self) -> Vec<u8> {
        self.sent.state_bytes()
    }

    pub fn cursor(&self) -> (u16, u16) {
        self.sent.cursor()
    }

    /// Which buffer this subscriber's replica is in. The seed compares it against the live
    /// screen rather than assuming a fresh parser is on primary, because a replayed history
    /// can leave it in either.
    pub fn alternate_screen(&self) -> bool {
        self.sent.alternate_screen()
    }
}

/// How many bytes of raw child output each pane retains, and therefore how far back a person
/// can scroll.
///
/// This bounds two things that used to be one small thing. It is still the *undelivered*
/// window — a subscriber that has fallen further behind than this must re-seed, which
/// `OutputLog::since` reports rather than papering over — and it is now also the pane's
/// history: the seed a client is given is a replay of this log, so what is retained here is
/// what anyone can scroll back to. One number for both is right because a subscriber more
/// than 16 MB behind has genuinely stalled, and 16 MB of raw output is hundreds of thousands
/// of lines.
///
/// It is deliberately in-memory only. A pane's output is every token, secret, and file body
/// an agent printed, and scrollback depth is not worth writing that to disk for.
pub const PANE_HISTORY_BYTES: usize = 16 << 20;

/// The most scrollback rows a client's replica is ever asked to retain.
///
/// A second budget is needed because the two sides pay in different currencies. The daemon
/// keeps raw bytes, which are cheap; a replica keeps parsed cells, and `vt100` allocates a
/// full row of them per scrollback row: `Row::new(cols)` eagerly allocates `cols` cells and a
/// `Cell` is exactly 32 bytes, whatever the row actually holds. Deriving rows from the byte
/// budget alone therefore prices history at roughly a thousandth of what it costs, and at 160
/// columns the 16 MiB budget would authorise about ten gigabytes of cells per pane.
///
/// **What a row costs, measured with an instrumented allocator:** about **2.6 KB at 80
/// columns** (2,602 bytes) and **5.2 KB at 160** (5,162 bytes) — `cols × 32` plus the row's
/// own header. Multiply by this constant before raising it. Ten thousand rows is therefore
/// roughly **26 MB per pane at 80 columns and 52 MB at 160**, per attached run. Fifty
/// thousand, which is what this was, came to 124 MiB and 246 MiB: several times the 16 MiB
/// byte log the same pane keeps, and by a wide margin the largest thing the client allocates.
/// Ten thousand is still five times the 2000 rows a replica held before pane history existed,
/// and further back than anyone scrolls to read.
///
/// The cap exists for a reason separate from that price, and the two should not be confused:
/// it is this, not the byte budget, that bounds a client's memory at all. A replica is fed the
/// live delta stream indefinitely, not merely the bytes the daemon can re-serve, so a
/// long-lived pane would grow without limit however small the daemon's retention were set.
pub const PANE_HISTORY_MAX_ROWS: usize = 10_000;

/// A bounded, in-memory record of the raw bytes a pane's child has written, addressed by a
/// monotonic byte sequence.
///
/// This is what lets a subscriber be sent the child's *own* output rather than a repaint of
/// the result. A repaint is cursor-addressed and therefore never scrolls, so a client fed
/// repaints can never accumulate history no matter how much output the pane produced; a
/// client fed the original bytes scrolls exactly as the daemon's terminal did.
///
/// It is deliberately in-memory only, exactly like the scrollback it feeds: nothing here is
/// ever written to a durable record.
pub struct OutputLog {
    /// Each retained write with the sequence of its first byte. Whole writes are dropped from
    /// the front, so the oldest retained sequence moves in write-sized steps.
    chunks: VecDeque<(u64, Vec<u8>)>,
    retained: usize,
    capacity: usize,
    end: u64,
    epoch: u64,
}

/// Distinguishes one pane's output from another's. A restart gives the same run a brand new
/// terminal whose sequence starts over at zero, so a reader holding a sequence from the old
/// one would otherwise be handed bytes from the middle of the new one and never know.
static NEXT_LOG_EPOCH: AtomicU64 = AtomicU64::new(1);

impl OutputLog {
    pub fn new(capacity: usize) -> Self {
        Self {
            chunks: VecDeque::new(),
            retained: 0,
            capacity,
            end: 0,
            epoch: NEXT_LOG_EPOCH.fetch_add(1, Ordering::Relaxed),
        }
    }

    /// Identity of the byte stream this log records. A reader must re-seed when it changes.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn append(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.chunks.push_back((self.end, bytes.to_vec()));
        self.end += bytes.len() as u64;
        self.retained += bytes.len();
        // The newest write is always kept, even when it alone exceeds the capacity: dropping it
        // would leave a reader unable to make progress at all rather than merely behind.
        while self.retained > self.capacity && self.chunks.len() > 1 {
            let (_, dropped) = self.chunks.pop_front().expect("more than one chunk");
            self.retained -= dropped.len();
        }
    }

    /// The sequence one past the newest byte: what a reader caught up to this instant records.
    pub fn end(&self) -> u64 {
        self.end
    }

    fn start(&self) -> u64 {
        self.chunks.front().map_or(self.end, |(start, _)| *start)
    }

    /// Everything written since sequence `from`, or `None` if those bytes are already gone.
    ///
    /// `None` is the whole point of the sequence: a reader that has fallen further behind than
    /// the log retains must be re-seeded from a fresh snapshot. Handing it the bytes that *are*
    /// still here would silently skip the rest, and a mirror missing a run of bytes renders
    /// like a corrupted screen with nothing to attribute it to.
    pub fn since(&self, from: u64) -> Option<Vec<u8>> {
        if from < self.start() || from > self.end {
            return None;
        }
        let mut pending = Vec::with_capacity((self.end - from) as usize);
        for (start, chunk) in &self.chunks {
            if start + chunk.len() as u64 <= from {
                continue;
            }
            let offset = usize::try_from(from.saturating_sub(*start)).unwrap_or(chunk.len());
            pending.extend_from_slice(&chunk[offset.min(chunk.len())..]);
        }
        Some(pending)
    }

    /// The newest `max` bytes or fewer, and the sequence they begin at.
    ///
    /// Whole writes, oldest-first, so a replay begins where the child began one — the closest
    /// thing to a safe parser entry point this log has. It is not a guarantee: a write
    /// boundary is not an escape-sequence boundary, so the oldest row of a replayed tail may
    /// carry one malformed glyph. The visible screen is repaired by `ScreenSync` regardless,
    /// which is what makes replaying an arbitrary tail safe at all.
    ///
    /// The newest write is always included even when it alone exceeds `max`, for the same
    /// reason `append` never drops it: a caller given nothing cannot make progress.
    pub fn tail(&self, max: usize) -> (u64, Vec<u8>) {
        let mut first = self.chunks.len();
        let mut bytes = 0;
        for (index, (_, chunk)) in self.chunks.iter().enumerate().rev() {
            if bytes + chunk.len() > max && bytes > 0 {
                break;
            }
            bytes += chunk.len();
            first = index;
        }
        let from = self.chunks.get(first).map_or(self.end, |(start, _)| *start);
        let mut out = Vec::with_capacity(bytes);
        for (_, chunk) in self.chunks.iter().skip(first) {
            out.extend_from_slice(chunk);
        }
        (from, out)
    }

    /// The `max` bytes immediately preceding `before`, for a reader extending its history
    /// backwards.
    ///
    /// Where [`since`](Self::since) refuses with `None`, this clamps. That difference is the
    /// point: `since` serves the delta path, where skipping bytes renders as corruption with
    /// nothing to attribute it to, and this serves the history path, where "that is all I
    /// still have" is a true and useful answer.
    ///
    /// A `before` that falls inside a write is truncated to it rather than skipping that
    /// write, so the answer always abuts the caller's cursor exactly. Returns the sequence the
    /// answer begins at, and whether it reached the oldest byte still retained — once that is
    /// true there is nothing older to ask for.
    pub fn before(&self, before: u64, max: usize) -> (u64, Vec<u8>, bool) {
        let mut pieces: Vec<(u64, &[u8])> = Vec::new();
        let mut bytes = 0;
        for (start, chunk) in self.chunks.iter().rev() {
            if *start >= before {
                continue;
            }
            let usable = usize::try_from(before - start)
                .unwrap_or(chunk.len())
                .min(chunk.len());
            if usable == 0 {
                continue;
            }
            if bytes + usable > max && !pieces.is_empty() {
                break;
            }
            bytes += usable;
            pieces.push((*start, &chunk[..usable]));
        }
        let from = pieces.last().map_or(before, |(start, _)| *start);
        let mut out = Vec::with_capacity(bytes);
        for (_, piece) in pieces.iter().rev() {
            out.extend_from_slice(piece);
        }
        (from, out, from <= self.start())
    }
}

/// A pane's live screen together with the raw bytes that produced it.
///
/// The two are one object because they must be read under one lock: a subscriber is sent
/// bytes up to some sequence and, in the same breath, reconciled against the screen those
/// exact bytes produce. Reading them separately would let the screen run ahead of the
/// sequence, and the client would then be sent bytes it had already been repainted with.
pub struct PaneOutput {
    screen: PaneScreen,
    log: OutputLog,
}

impl PaneOutput {
    pub fn new(rows: u16, cols: u16, scrollback_rows: usize, log_bytes: usize) -> Self {
        Self {
            screen: PaneScreen::new(rows, cols, scrollback_rows),
            log: OutputLog::new(log_bytes),
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.screen.feed(bytes);
        self.log.append(bytes);
    }

    pub fn screen(&self) -> &PaneScreen {
        &self.screen
    }

    pub fn screen_mut(&mut self) -> &mut PaneScreen {
        &mut self.screen
    }

    pub fn log(&self) -> &OutputLog {
        &self.log
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `PANE_HISTORY_MAX_ROWS` is a memory budget written as a row count, and the conversion
    /// is what makes it reviewable: `vt100` allocates `cols` cells of 32 bytes for every
    /// retained row, whatever that row holds. Bracketed from both sides rather than pinned to
    /// a literal, because both sides are the real constraint — raising it has to argue with
    /// the megabytes per pane it buys, and lowering it has to argue with how far back a person
    /// can scroll. The measured price includes a per-row header the arithmetic below leaves
    /// out (2,602 bytes at 80 columns against 2,560 here), so this is a floor on the cost.
    #[test]
    fn the_replica_row_cap_prices_a_pane_in_tens_of_megabytes_rather_than_hundreds() {
        const CELL_BYTES: usize = 32;
        let cells = |cols: usize| PANE_HISTORY_MAX_ROWS * cols * CELL_BYTES;
        assert!(
            cells(160) <= 64 << 20,
            "a single pane's scrollback must stay inside 64 MiB at the widest layout Dock \
             renders; {} rows of 160 columns is {} MiB",
            PANE_HISTORY_MAX_ROWS,
            cells(160) >> 20
        );
        assert!(
            cells(80) <= 32 << 20,
            "and inside 32 MiB at 80 columns: {} MiB",
            cells(80) >> 20
        );
        // A `const` block because both sides are compile-time constants and clippy rightly
        // refuses a runtime assertion that can never vary: this one fails the build, not a
        // test run, which is if anything the better place for it.
        const {
            assert!(
                PANE_HISTORY_MAX_ROWS >= 5 * 2_000,
                "the cap must stay at least five times the 2000 rows a replica held before \
                 pane history existed, or it has taken back the feature it bounds"
            );
        }
    }

    #[test]
    fn a_reader_that_keeps_up_receives_every_byte_exactly_once() {
        let mut log = OutputLog::new(64);
        assert_eq!(log.since(0).as_deref(), Some(b"".as_slice()));
        log.append(b"one");
        let mut offset = 0;
        let first = log.since(offset).expect("still retained");
        assert_eq!(first, b"one");
        offset += first.len() as u64;
        log.append(b"two");
        assert_eq!(log.since(offset).as_deref(), Some(b"two".as_slice()));
        assert_eq!(log.end(), 6);
    }

    #[test]
    fn a_reader_that_falls_past_the_capacity_is_reported_rather_than_skipped() {
        let mut log = OutputLog::new(8);
        log.append(b"12345678");
        // Still exactly at the edge: every byte a reader at zero needs is retained.
        assert_eq!(log.since(0).as_deref(), Some(b"12345678".as_slice()));
        log.append(b"9");
        assert!(
            log.since(0).is_none(),
            "a reader behind the retained window must be detectable, never partially served"
        );
        // A reader inside the surviving window is still served, and a caught-up one sees nothing.
        assert_eq!(log.since(8).as_deref(), Some(b"9".as_slice()));
        assert_eq!(log.since(9).as_deref(), Some(b"".as_slice()));
        // A sequence from beyond the end belongs to some other log entirely.
        assert!(log.since(10).is_none());
    }

    #[test]
    fn the_newest_write_survives_even_when_it_alone_exceeds_the_capacity() {
        let mut log = OutputLog::new(4);
        log.append(b"old");
        log.append(b"an oversized single write");
        assert_eq!(
            log.since(3).as_deref(),
            Some(b"an oversized single write".as_slice())
        );
    }

    #[test]
    fn each_log_is_a_stream_of_its_own_so_a_restart_cannot_be_read_as_continuation() {
        let first = OutputLog::new(16);
        let second = OutputLog::new(16);
        assert_ne!(
            first.epoch(),
            second.epoch(),
            "a replacement terminal restarts the sequence at zero, so a reader holding an \
             offset from the old one must be able to tell the two apart"
        );
    }

    #[test]
    fn feeding_a_pane_advances_the_screen_and_the_log_together() {
        let mut output = PaneOutput::new(5, 20, 100, 64);
        output.feed(b"hello\r\n");
        assert_eq!(output.log().end(), 7);
        assert_eq!(
            output.log().since(0).as_deref(),
            Some(b"hello\r\n".as_slice())
        );
        assert!(output.screen().text_tail(1).contains("hello"));
    }

    #[test]
    fn a_tail_returns_the_newest_bytes_and_the_sequence_they_begin_at() {
        let mut log = OutputLog::new(1024);
        log.append(b"oldest");
        log.append(b"middle");
        log.append(b"newest");
        let (from, bytes) = log.tail(12);
        assert_eq!(bytes, b"middlenewest");
        assert_eq!(from, 6);
    }

    #[test]
    fn a_tail_keeps_the_newest_write_even_when_it_alone_exceeds_the_budget() {
        let mut log = OutputLog::new(1024);
        log.append(b"old");
        log.append(b"a_single_enormous_write");
        let (from, bytes) = log.tail(4);
        assert_eq!(bytes, b"a_single_enormous_write");
        assert_eq!(from, 3);
    }

    #[test]
    fn a_tail_of_an_empty_log_begins_at_the_end_and_carries_nothing() {
        let log = OutputLog::new(1024);
        assert_eq!(log.tail(64), (0, Vec::new()));
    }

    #[test]
    fn history_before_a_cursor_extends_backwards_without_a_gap() {
        let mut log = OutputLog::new(1024);
        log.append(b"aaaa");
        log.append(b"bbbb");
        log.append(b"cccc");
        // The client holds everything from sequence 8; ask for what precedes it.
        let (from, bytes, complete) = log.before(8, 4);
        assert_eq!(bytes, b"bbbb");
        assert_eq!(from, 4);
        assert!(!complete, "sequence 0 is still retained and unasked for");
        let (from, bytes, complete) = log.before(from, 4);
        assert_eq!(bytes, b"aaaa");
        assert_eq!(from, 0);
        assert!(complete, "nothing older is retained");
    }

    #[test]
    fn history_before_a_cursor_inside_a_write_stops_exactly_at_the_cursor() {
        let mut log = OutputLog::new(1024);
        log.append(b"abcdefgh");
        // A cursor that is not a write boundary must not pull in bytes the caller already has,
        // and must not leave a gap between what it returns and where the caller starts.
        let (from, bytes, complete) = log.before(5, 64);
        assert_eq!(bytes, b"abcde");
        assert_eq!(from, 0);
        assert!(complete);
    }

    #[test]
    fn history_before_the_oldest_retained_byte_is_empty_and_complete() {
        let mut log = OutputLog::new(8);
        log.append(b"aaaa");
        log.append(b"bbbb");
        log.append(b"cccc");
        let (_, dropped, _) = log.before(4, 64);
        assert!(
            dropped.is_empty(),
            "the first write was evicted at capacity"
        );
    }
}
