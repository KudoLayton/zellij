//! Generation and bounded subscriber history adapted from the fork's
//! b7cc89060, baa1e1284, 3687944da and 1b187d8eb.
//! History eviction never drops bytes on the PTY -> terminal parser path.
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
};

static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug)]
pub(crate) struct TerminalStreamGuard {
    live: Arc<AtomicBool>,
    generation: u64,
}
impl TerminalStreamGuard {
    pub(crate) fn new() -> Self {
        Self {
            live: Arc::new(AtomicBool::new(true)),
            generation: NEXT_GENERATION.fetch_add(1, Ordering::Relaxed),
        }
    }
    pub(crate) fn invalidate(&self) {
        self.live.store(false, Ordering::Release);
    }
    pub(crate) fn is_current(&self) -> bool {
        self.live.load(Ordering::Acquire)
    }
}

/// Internal screen instruction payload; never serialized to clients/plugins.
#[derive(Clone, Debug)]
pub struct TerminalOutput {
    pub(crate) terminal_id: u32,
    pub(crate) bytes: Vec<u8>,
    pub(crate) sequence: u64,
    guard: TerminalStreamGuard,
}
impl TerminalOutput {
    pub(crate) fn new(
        terminal_id: u32,
        bytes: Vec<u8>,
        sequence: u64,
        guard: TerminalStreamGuard,
    ) -> Self {
        Self {
            terminal_id,
            bytes,
            sequence,
            guard,
        }
    }
    pub(crate) fn is_current(&self) -> bool {
        self.guard.is_current()
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TerminalOutputCursor {
    generation: Option<u64>,
    next_sequence: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TerminalOutputGap {
    pub(crate) expected_sequence: u64,
    pub(crate) resume_sequence: u64,
    pub(crate) skipped_events: u64,
    pub(crate) newest_sequence: u64,
    pub(crate) recent_bytes: Vec<u8>,
    pub(crate) generation_changed: bool,
}

#[derive(Debug)]
struct Ring {
    generation: u64,
    // Subscriber recovery needs sequence metadata, not duplicate payloads.
    sequences: VecDeque<u64>,
    recent_bytes: VecDeque<u8>,
    newest: u64,
}

#[derive(Debug)]
pub(crate) struct TerminalOutputStore {
    rings: HashMap<u32, Ring>,
    event_capacity: usize,
    recent_byte_capacity: usize,
}
impl TerminalOutputStore {
    pub(crate) fn new() -> Self {
        Self::with_limits(1024, 1024 * 1024)
    }
    fn with_limits(event_capacity: usize, recent_byte_capacity: usize) -> Self {
        Self {
            rings: HashMap::new(),
            event_capacity: event_capacity.max(1),
            recent_byte_capacity,
        }
    }
    /// Returns false for stale/replayed bytes. Call only once the pane exists,
    /// immediately before passing its bytes to the terminal parser.
    pub(crate) fn push(&mut self, output: &TerminalOutput) -> bool {
        if !output.is_current() {
            return false;
        }
        let generation = output.guard.generation;
        if let Some(ring) = self.rings.get(&output.terminal_id) {
            if generation < ring.generation
                || (generation == ring.generation && output.sequence <= ring.newest)
            {
                return false;
            }
        }
        let ring = self
            .rings
            .entry(output.terminal_id)
            .or_insert_with(|| Ring {
                generation,
                sequences: VecDeque::new(),
                recent_bytes: VecDeque::new(),
                newest: output.sequence,
            });
        if ring.generation != generation {
            ring.generation = generation;
            ring.sequences.clear();
            ring.recent_bytes.clear();
        }
        ring.newest = output.sequence;
        ring.sequences.push_back(output.sequence);
        while ring.sequences.len() > self.event_capacity {
            ring.sequences.pop_front();
        }
        // Never temporarily append an arbitrarily large payload to the ring.
        let bytes = &output.bytes[output.bytes.len().saturating_sub(self.recent_byte_capacity)..];
        let keep = self.recent_byte_capacity - bytes.len();
        let discard = ring.recent_bytes.len().saturating_sub(keep);
        ring.recent_bytes.drain(..discard);
        ring.recent_bytes.extend(bytes);
        true
    }
    pub(crate) fn remove(&mut self, terminal_id: u32) {
        self.rings.remove(&terminal_id);
    }
    pub(crate) fn cursor_from_now(&self, terminal_id: u32) -> TerminalOutputCursor {
        self.rings
            .get(&terminal_id)
            .map(|ring| TerminalOutputCursor {
                generation: Some(ring.generation),
                next_sequence: ring.newest.saturating_add(1),
            })
            .unwrap_or_default()
    }
    /// Advance a render subscriber to current state. Any eviction, sequence
    /// hole, or generation change requires a full pane snapshot, not raw-byte
    /// replay (a byte suffix need not begin at a VT/UTF-8 boundary).
    pub(crate) fn advance(
        &self,
        terminal_id: u32,
        cursor: &mut TerminalOutputCursor,
    ) -> Option<TerminalOutputGap> {
        let ring = self.rings.get(&terminal_id)?;
        let changed = cursor.generation.is_some_and(|g| g != ring.generation);
        let expected = cursor.next_sequence;
        let start = if changed { 0 } else { expected };
        let mut next = start;
        let mut first_gap = None;
        for &sequence in &ring.sequences {
            if sequence < next {
                continue;
            }
            if sequence > next && first_gap.is_none() {
                first_gap = Some((next, sequence));
            }
            next = sequence.saturating_add(1);
        }
        *cursor = self.cursor_from_now(terminal_id);
        if changed || first_gap.is_some() {
            let (gap_start, resume) = first_gap.unwrap_or((expected, start));
            Some(TerminalOutputGap {
                expected_sequence: gap_start,
                resume_sequence: resume,
                skipped_events: resume.saturating_sub(gap_start),
                newest_sequence: ring.newest,
                recent_bytes: ring.recent_bytes.iter().copied().collect(),
                generation_changed: changed,
            })
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn output(guard: &TerminalStreamGuard, sequence: u64, bytes: &[u8]) -> TerminalOutput {
        TerminalOutput::new(1, bytes.to_vec(), sequence, guard.clone())
    }
    #[test]
    fn invalidated_generation_rejects_queued_bytes_before_new_output() {
        let guard = TerminalStreamGuard::new();
        let queued = output(&guard, 0, b"stale");
        guard.invalidate();
        assert!(!TerminalOutputStore::new().push(&queued));
        let fresh = TerminalStreamGuard::new();
        assert!(fresh.is_current());
        guard.invalidate();
        assert!(fresh.is_current());
    }
    #[test]
    fn rejects_duplicate_out_of_order_and_older_generation() {
        let mut store = TerminalOutputStore::new();
        let old = TerminalStreamGuard::new();
        let new = TerminalStreamGuard::new();
        assert!(store.push(&output(&old, 1, b"one")));
        assert!(!store.push(&output(&old, 1, b"duplicate")));
        assert!(!store.push(&output(&old, 0, b"earlier")));
        assert!(store.push(&output(&new, 0, b"new")));
        assert!(!store.push(&output(&old, 10, b"late")));
    }
    #[test]
    fn sequential_output_does_not_require_recovery() {
        let mut store = TerminalOutputStore::new();
        let guard = TerminalStreamGuard::new();
        let mut cursor = store.cursor_from_now(1);
        for seq in 0..5 {
            store.push(&output(&guard, seq, b"ok"));
        }
        assert!(store.advance(1, &mut cursor).is_none());
        assert!(store.advance(1, &mut cursor).is_none());
    }
    #[test]
    fn eviction_preserves_gap_metadata_and_bounded_recent_bytes() {
        let mut store = TerminalOutputStore::with_limits(2, 5);
        let guard = TerminalStreamGuard::new();
        let mut cursor = store.cursor_from_now(1);
        for seq in 0..4 {
            store.push(&output(&guard, seq, b"abc"));
        }
        let gap = store.advance(1, &mut cursor).unwrap();
        assert_eq!(
            (
                gap.expected_sequence,
                gap.resume_sequence,
                gap.skipped_events,
                gap.newest_sequence
            ),
            (0, 2, 2, 3)
        );
        assert_eq!(gap.recent_bytes, b"bcabc");
        assert!(store.advance(1, &mut cursor).is_none());
    }
    #[test]
    fn detects_internal_sequence_hole_not_just_eviction() {
        let mut store = TerminalOutputStore::new();
        let guard = TerminalStreamGuard::new();
        let mut cursor = store.cursor_from_now(1);
        store.push(&output(&guard, 0, b"a"));
        store.push(&output(&guard, 2, b"c"));
        let gap = store.advance(1, &mut cursor).unwrap();
        assert_eq!(
            (
                gap.expected_sequence,
                gap.resume_sequence,
                gap.skipped_events
            ),
            (1, 2, 1)
        );
    }
    #[test]
    fn new_generation_recovers_even_if_old_cursor_is_ahead() {
        let mut store = TerminalOutputStore::new();
        let old = TerminalStreamGuard::new();
        store.push(&output(&old, 99, b"old"));
        let mut cursor = store.cursor_from_now(1);
        store.push(&output(&TerminalStreamGuard::new(), 0, b"fresh"));
        let gap = store.advance(1, &mut cursor).unwrap();
        assert!(gap.generation_changed);
        assert_eq!(gap.recent_bytes, b"fresh");
    }
    #[test]
    fn remove_frees_history_and_oversized_events_obey_byte_limit() {
        let mut store = TerminalOutputStore::with_limits(1, 3);
        store.push(&output(&TerminalStreamGuard::new(), 0, b"0123456789"));
        assert_eq!(
            store.rings[&1]
                .recent_bytes
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            b"789"
        );
        store.remove(1);
        assert!(store.rings.is_empty());
    }
}
