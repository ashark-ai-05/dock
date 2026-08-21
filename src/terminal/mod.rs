mod keys;
mod vt;

pub use keys::{KeyEncoding, encode_key, encode_paste};
pub use vt::{ShellSignals, VtTerminal};

/// Single swap point for the terminal engine. `rio-vt` can replace `VtTerminal` here
/// without touching any caller once it is mature enough to depend on.
pub type PaneScreen = VtTerminal;

/// Tracks what a single subscriber has already been sent, so the daemon can transmit
/// only the difference. An unchanged screen yields an empty delta and therefore no traffic.
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
