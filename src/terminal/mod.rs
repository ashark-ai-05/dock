mod keys;
mod vt;

use std::{
    collections::VecDeque,
    sync::atomic::{AtomicU64, Ordering},
};

pub use keys::{KeyEncoding, encode_key, encode_paste};
pub use vt::{ShellSignals, VtTerminal};

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
}

/// How many bytes of raw child output each pane retains for subscribers that have not read
/// them yet. It bounds only the *undelivered* window, not history: the scrollback the user
/// scrolls through lives in each side's parser, not here. A megabyte is far more than the
/// 16 ms between stream polls can produce for any interactive program, so overrunning it
/// means a subscriber has genuinely stalled — which `OutputLog::since` reports rather than
/// papering over.
pub const PANE_OUTPUT_LOG_BYTES: usize = 1 << 20;

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
}
