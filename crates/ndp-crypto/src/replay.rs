//! Replay protection.

/// A sliding bitmap of recently accepted sequence numbers.
///
/// Reliable channels only ever advance, but audio rides datagrams where
/// reordering is normal, so a strict "must be greater than the last" rule
/// would discard perfectly good packets. A 64-entry window accepts genuine
/// reordering while still rejecting replays.
#[derive(Debug, Clone)]
pub struct ReplayWindow {
    highest: u64,
    bitmap: u64,
    started: bool,
    width: u32,
}

impl ReplayWindow {
    /// How far behind the newest sequence a record may arrive.
    pub const DEFAULT_WIDTH: u32 = 64;

    /// A window that tolerates reordering up to [`Self::DEFAULT_WIDTH`].
    #[must_use]
    pub fn new() -> Self {
        Self::with_width(Self::DEFAULT_WIDTH)
    }

    /// A window that requires strictly increasing sequences — correct for
    /// reliable, ordered channels where reordering cannot happen.
    #[must_use]
    pub fn strict() -> Self {
        Self::with_width(0)
    }

    /// A window of the given width, capped at 64.
    #[must_use]
    pub fn with_width(width: u32) -> Self {
        Self {
            highest: 0,
            bitmap: 0,
            started: false,
            width: width.min(64),
        }
    }

    /// Test whether `seq` would be accepted, without recording it.
    #[must_use]
    pub fn would_accept(&self, seq: u64) -> bool {
        if !self.started {
            return true;
        }
        if seq > self.highest {
            return true;
        }
        let age = self.highest - seq;
        if age >= u64::from(self.width).max(1) {
            return false;
        }
        self.bitmap & (1u64 << age) == 0
    }

    /// Accept `seq` and record it. Returns `false` if it is a replay.
    pub fn accept(&mut self, seq: u64) -> bool {
        if !self.started {
            self.started = true;
            self.highest = seq;
            self.bitmap = 1;
            return true;
        }
        if seq > self.highest {
            let shift = seq - self.highest;
            self.bitmap = if shift >= 64 {
                0
            } else {
                self.bitmap << shift
            };
            self.bitmap |= 1;
            self.highest = seq;
            return true;
        }
        if !self.would_accept(seq) {
            return false;
        }
        let age = self.highest - seq;
        self.bitmap |= 1u64 << age;
        true
    }

    /// The highest sequence accepted so far.
    #[must_use]
    pub const fn highest(&self) -> u64 {
        self.highest
    }
}

impl Default for ReplayWindow {
    fn default() -> Self {
        Self::new()
    }
}

/// Expands a 32-bit wire sequence into the full 64-bit counter used as the
/// AEAD nonce, using the SRTP roll-over-counter estimation rule.
#[derive(Debug, Clone, Default)]
pub struct SeqExpander {
    highest: u64,
    started: bool,
}

impl SeqExpander {
    /// A fresh expander positioned before the first record.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            highest: 0,
            started: false,
        }
    }

    /// Estimate the full sequence for a wire value, choosing the roll-over
    /// counter that places the result closest to the last-seen sequence.
    ///
    /// This does *not* mutate state — the caller only commits via
    /// [`Self::commit`] after the record authenticates, so a forged sequence
    /// cannot poison the estimator.
    #[must_use]
    pub fn expand(&self, wire: u32) -> u64 {
        if !self.started {
            return u64::from(wire);
        }
        let roc = self.highest >> 32;
        let low = self.highest as u32;
        let wire64 = u64::from(wire);

        let candidate = |roc: u64| (roc << 32) | wire64;
        let same = candidate(roc);
        let next = candidate(roc.wrapping_add(1));
        let prev = if roc == 0 { same } else { candidate(roc - 1) };

        let dist = |v: u64| v.abs_diff(self.highest);
        if low >= 0x8000_0000 && wire < 0x8000_0000 && dist(next) < dist(same) {
            next
        } else if low < 0x8000_0000 && wire >= 0x8000_0000 && dist(prev) < dist(same) {
            prev
        } else {
            same
        }
    }

    /// Record an authenticated sequence so future estimates track it.
    pub fn commit(&mut self, full: u64) {
        if !self.started || full > self.highest {
            self.highest = full;
            self.started = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_window_accepts_anything_once() {
        let mut w = ReplayWindow::new();
        assert!(w.accept(1000));
        assert!(!w.accept(1000));
    }

    #[test]
    fn window_tolerates_reordering_but_rejects_replays() {
        let mut w = ReplayWindow::new();
        assert!(w.accept(10));
        assert!(w.accept(12));
        assert!(w.accept(11), "reordered packet must be accepted");
        assert!(!w.accept(11), "but only once");
        assert_eq!(w.highest(), 12);
    }

    #[test]
    fn window_rejects_records_older_than_its_width() {
        let mut w = ReplayWindow::with_width(8);
        assert!(w.accept(100));
        assert!(w.accept(95));
        assert!(!w.accept(80));
    }

    #[test]
    fn strict_window_requires_increasing_sequences() {
        let mut w = ReplayWindow::strict();
        assert!(w.accept(1));
        assert!(w.accept(2));
        assert!(!w.accept(1));
        assert!(!w.accept(2));
        assert!(w.accept(3));
    }

    #[test]
    fn large_jump_clears_the_bitmap() {
        let mut w = ReplayWindow::new();
        assert!(w.accept(1));
        assert!(w.accept(1_000_000));
        assert!(!w.accept(1), "old sequence is far outside the window");
        assert!(w.accept(999_999));
    }

    #[test]
    fn expander_passes_through_before_rollover() {
        let mut e = SeqExpander::new();
        for seq in [0u32, 1, 2, 100, 5000] {
            let full = e.expand(seq);
            assert_eq!(full, u64::from(seq));
            e.commit(full);
        }
    }

    #[test]
    fn expander_detects_rollover() {
        let mut e = SeqExpander::new();
        let near_max = u32::MAX - 2;
        let full = e.expand(near_max);
        e.commit(full);
        assert_eq!(full, u64::from(near_max));

        // Wire counter wraps to 1; the full counter must climb past 2^32.
        let wrapped = e.expand(1);
        assert_eq!(wrapped, (1u64 << 32) | 1);
        e.commit(wrapped);

        // A late packet from before the wrap must map back below 2^32.
        assert_eq!(e.expand(near_max), u64::from(near_max));
    }

    #[test]
    fn expander_ignores_regressions_when_committing() {
        let mut e = SeqExpander::new();
        e.commit(500);
        e.commit(10);
        assert_eq!(e.expand(600), 600);
    }
}
