use crate::terminal_bytes::TerminalOutput;
use std::collections::{HashMap, VecDeque};

const DEFAULT_EVENT_CAPACITY: usize = 1024;
const DEFAULT_RECENT_BYTE_CAPACITY: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TerminalOutputEvent {
    pub(crate) sequence: u64,
    pub(crate) bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TerminalOutputCursor {
    next_sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TerminalOutputGap {
    pub(crate) expected_sequence: u64,
    pub(crate) resume_sequence: u64,
    pub(crate) skipped_events: u64,
    pub(crate) newest_sequence: u64,
    pub(crate) recent_bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TerminalOutputCursorItem {
    Event(TerminalOutputEvent),
    Gap(TerminalOutputGap),
}

#[derive(Debug)]
pub(crate) struct TerminalOutputStore {
    terminals: HashMap<u32, TerminalOutputRing>,
    event_capacity: usize,
    recent_byte_capacity: usize,
}

#[derive(Debug)]
struct TerminalOutputRing {
    generation: u64,
    events: VecDeque<TerminalOutputEvent>,
    recent_bytes: VecDeque<u8>,
    recent_byte_capacity: usize,
}

impl TerminalOutputStore {
    pub(crate) fn new() -> Self {
        Self::with_limits(DEFAULT_EVENT_CAPACITY, DEFAULT_RECENT_BYTE_CAPACITY)
    }

    pub(crate) fn with_limits(event_capacity: usize, recent_byte_capacity: usize) -> Self {
        Self {
            terminals: HashMap::new(),
            event_capacity: event_capacity.max(1),
            recent_byte_capacity,
        }
    }

    pub(crate) fn push(&mut self, output: &TerminalOutput) -> Option<u64> {
        let generation = output.generation?;
        let sequence = output.sequence?;
        let ring = self
            .terminals
            .entry(output.terminal_id)
            .or_insert_with(|| TerminalOutputRing::new(generation, self.recent_byte_capacity));
        ring.push(
            generation,
            sequence,
            output.bytes.clone(),
            self.event_capacity,
        )
    }

    #[cfg(test)]
    fn cursor_from_oldest(&self, terminal_id: u32) -> TerminalOutputCursor {
        self.terminals
            .get(&terminal_id)
            .map(TerminalOutputRing::cursor_from_oldest)
            .unwrap_or(TerminalOutputCursor { next_sequence: 0 })
    }

    pub(crate) fn cursor_from_now(&self, terminal_id: u32) -> TerminalOutputCursor {
        self.terminals
            .get(&terminal_id)
            .map(TerminalOutputRing::cursor_from_now)
            .unwrap_or(TerminalOutputCursor { next_sequence: 0 })
    }

    pub(crate) fn poll_cursor(
        &self,
        terminal_id: u32,
        cursor: &mut TerminalOutputCursor,
    ) -> Option<TerminalOutputCursorItem> {
        self.terminals
            .get(&terminal_id)
            .and_then(|ring| ring.poll_cursor(cursor))
    }

    pub(crate) fn drain_cursor(
        &self,
        terminal_id: u32,
        cursor: &mut TerminalOutputCursor,
        limit: usize,
    ) -> Vec<TerminalOutputCursorItem> {
        let mut drained = Vec::new();
        for _ in 0..limit {
            let Some(item) = self.poll_cursor(terminal_id, cursor) else {
                break;
            };
            let is_gap = matches!(item, TerminalOutputCursorItem::Gap(_));
            drained.push(item);
            if is_gap {
                break;
            }
        }
        drained
    }
}

impl TerminalOutputRing {
    fn new(generation: u64, recent_byte_capacity: usize) -> Self {
        Self {
            generation,
            events: VecDeque::new(),
            recent_bytes: VecDeque::new(),
            recent_byte_capacity,
        }
    }

    fn push(
        &mut self,
        generation: u64,
        sequence: u64,
        bytes: Vec<u8>,
        event_capacity: usize,
    ) -> Option<u64> {
        if generation < self.generation {
            return None;
        }
        if generation > self.generation {
            self.generation = generation;
            self.events.clear();
            self.recent_bytes.clear();
        }
        if self
            .events
            .back()
            .is_some_and(|event| sequence <= event.sequence)
        {
            return None;
        }
        self.append_recent_bytes(&bytes);
        self.events
            .push_back(TerminalOutputEvent { sequence, bytes });
        while self.events.len() > event_capacity {
            self.events.pop_front();
        }
        Some(sequence)
    }

    #[cfg(test)]
    fn cursor_from_oldest(&self) -> TerminalOutputCursor {
        TerminalOutputCursor {
            next_sequence: self.oldest_sequence(),
        }
    }

    fn cursor_from_now(&self) -> TerminalOutputCursor {
        TerminalOutputCursor {
            next_sequence: self.next_sequence(),
        }
    }

    fn poll_cursor(&self, cursor: &mut TerminalOutputCursor) -> Option<TerminalOutputCursorItem> {
        if self.events.is_empty() {
            return None;
        }
        let oldest_sequence = self.oldest_sequence();
        if cursor.next_sequence < oldest_sequence {
            let expected_sequence = cursor.next_sequence;
            cursor.next_sequence = oldest_sequence;
            return Some(TerminalOutputCursorItem::Gap(TerminalOutputGap {
                expected_sequence,
                resume_sequence: oldest_sequence,
                skipped_events: oldest_sequence.saturating_sub(expected_sequence),
                newest_sequence: self.newest_sequence(),
                recent_bytes: self.recent_bytes.iter().copied().collect(),
            }));
        }
        let event = self
            .events
            .iter()
            .find(|event| event.sequence == cursor.next_sequence)?;
        cursor.next_sequence = cursor.next_sequence.saturating_add(1);
        Some(TerminalOutputCursorItem::Event(event.clone()))
    }

    fn oldest_sequence(&self) -> u64 {
        self.events.front().map(|event| event.sequence).unwrap_or(0)
    }

    fn next_sequence(&self) -> u64 {
        self.events
            .back()
            .map(|event| event.sequence.saturating_add(1))
            .unwrap_or(0)
    }

    fn newest_sequence(&self) -> u64 {
        self.events.back().map(|event| event.sequence).unwrap_or(0)
    }

    fn append_recent_bytes(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.recent_bytes.push_back(*byte);
        }
        while self.recent_bytes.len() > self.recent_byte_capacity {
            self.recent_bytes.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(terminal_id: u32, generation: u64, sequence: u64, bytes: &[u8]) -> TerminalOutput {
        TerminalOutput::guarded(terminal_id, bytes.to_vec(), generation, sequence)
    }

    #[test]
    fn cursor_reads_retained_output_in_sequence_order() {
        let mut store = TerminalOutputStore::with_limits(4, 64);
        store.push(&output(1, 1, 0, b"one"));
        store.push(&output(1, 1, 1, b"two"));
        let mut cursor = store.cursor_from_oldest(1);

        assert_eq!(
            store.poll_cursor(1, &mut cursor),
            Some(TerminalOutputCursorItem::Event(TerminalOutputEvent {
                sequence: 0,
                bytes: b"one".to_vec(),
            }))
        );
        assert_eq!(
            store.poll_cursor(1, &mut cursor),
            Some(TerminalOutputCursorItem::Event(TerminalOutputEvent {
                sequence: 1,
                bytes: b"two".to_vec(),
            }))
        );
        assert_eq!(store.poll_cursor(1, &mut cursor), None);
    }

    #[test]
    fn cursor_reports_lag_when_retention_rotates() {
        let mut store = TerminalOutputStore::with_limits(2, 6);
        store.push(&output(1, 1, 0, b"aa"));
        let mut cursor = store.cursor_from_oldest(1);
        store.push(&output(1, 1, 1, b"bb"));
        store.push(&output(1, 1, 2, b"cc"));

        assert_eq!(
            store.poll_cursor(1, &mut cursor),
            Some(TerminalOutputCursorItem::Gap(TerminalOutputGap {
                expected_sequence: 0,
                resume_sequence: 1,
                skipped_events: 1,
                newest_sequence: 2,
                recent_bytes: b"aabbcc".to_vec(),
            }))
        );
    }

    #[test]
    fn drain_cursor_reports_gap_once_then_resumes_at_oldest() {
        let mut store = TerminalOutputStore::with_limits(2, 64);
        let mut cursor = store.cursor_from_oldest(1);
        store.push(&output(1, 1, 0, b"zero"));
        store.push(&output(1, 1, 1, b"one"));
        store.push(&output(1, 1, 2, b"two"));

        let first_batch = store.drain_cursor(1, &mut cursor, 8);
        assert_eq!(
            first_batch,
            vec![TerminalOutputCursorItem::Gap(TerminalOutputGap {
                expected_sequence: 0,
                resume_sequence: 1,
                skipped_events: 1,
                newest_sequence: 2,
                recent_bytes: b"zeroonetwo".to_vec(),
            })]
        );

        let second_batch = store.drain_cursor(1, &mut cursor, 8);
        assert_eq!(
            second_batch,
            vec![
                TerminalOutputCursorItem::Event(TerminalOutputEvent {
                    sequence: 1,
                    bytes: b"one".to_vec(),
                }),
                TerminalOutputCursorItem::Event(TerminalOutputEvent {
                    sequence: 2,
                    bytes: b"two".to_vec(),
                }),
            ]
        );
    }

    #[test]
    fn cursor_from_now_skips_already_retained_output() {
        let mut store = TerminalOutputStore::with_limits(4, 64);
        store.push(&output(1, 1, 0, b"old"));
        let mut cursor = store.cursor_from_now(1);
        assert_eq!(store.poll_cursor(1, &mut cursor), None);

        store.push(&output(1, 1, 1, b"new"));
        assert_eq!(
            store.poll_cursor(1, &mut cursor),
            Some(TerminalOutputCursorItem::Event(TerminalOutputEvent {
                sequence: 1,
                bytes: b"new".to_vec(),
            }))
        );
    }

    #[test]
    fn generation_switch_clears_retained_output_and_rejects_stale_output() {
        let mut store = TerminalOutputStore::with_limits(4, 64);
        store.push(&output(1, 1, 0, b"old"));
        store.push(&output(1, 2, 0, b"fresh"));
        assert_eq!(store.push(&output(1, 1, 1, b"stale")), None);
        let mut cursor = store.cursor_from_oldest(1);

        assert_eq!(
            store.poll_cursor(1, &mut cursor),
            Some(TerminalOutputCursorItem::Event(TerminalOutputEvent {
                sequence: 0,
                bytes: b"fresh".to_vec(),
            }))
        );
        assert_eq!(store.poll_cursor(1, &mut cursor), None);
    }

    #[test]
    fn unguarded_output_is_not_retained() {
        let mut store = TerminalOutputStore::with_limits(4, 64);
        assert_eq!(
            store.push(&TerminalOutput::unguarded(1, b"internal".to_vec())),
            None
        );
    }
}
