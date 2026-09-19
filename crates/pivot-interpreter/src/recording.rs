//! The **verbatim per-leg recording** (`PCAP2TEST_PIVOT_V3.md` §14 item 10):
//! every message, in wire order, with its arrival time, in every mode, on every
//! lane, whether or not anything asserted.
//!
//! The recording is the run's evidence, so it is held in memory as it is made
//! and written by a handle that OUTLIVES the run: a panicking run still leaves
//! its ladder on disk. A datagram is recorded as the BYTES that crossed the
//! socket (ADR-0035); a repeat is recognised by those bytes. Attribution is
//! best-effort by design — a datagram no step claimed is recorded with no
//! `step`, never dropped — but the datagram itself never is.
//!
//! This is the MAKING of a recording; the line it writes is
//! [`pivot_schema::bundle::RecordedMessage`].

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use pivot_schema::bundle::{Dir, RecordedMessage};

/// One datagram as its recorder states it, before the leg numbers it: `seq` is
/// the log's to assign, never a caller's.
struct Entry<'a> {
    dir: Dir,
    at_us: u64,
    wire: Vec<u8>,
    step: Option<&'a str>,
    note: Option<&'a str>,
    repeat_of: Option<u64>,
}

/// The in-memory recording, shared with the writer that outlives the run.
#[derive(Debug, Default)]
struct Log {
    by_leg: BTreeMap<String, Vec<RecordedMessage>>,
}

/// A handle onto one run's recording. Cloneable and `Send`-free by design: the
/// harness is single-threaded, and the handle's job is to survive a panic in the
/// run body, not to cross threads.
#[derive(Debug, Clone, Default)]
pub struct Recording {
    log: Arc<Mutex<Log>>,
}

impl Recording {
    pub fn new() -> Self {
        Recording::default()
    }

    /// Declare a leg the run BOUND a socket for, before it has heard anything.
    ///
    /// An empty ladder and a missing one say different things — "bound, heard
    /// nothing" versus "the run never got that far" — and only the first is
    /// evidence. Declaring is idempotent and never disturbs what is there.
    pub fn declare(&self, leg: &str) {
        let mut log = self.log.lock().expect("the recording lock outlives its critical sections");
        log.by_leg.entry(leg.to_string()).or_default();
    }

    /// Record one datagram on `leg`, byte for byte. Always succeeds: a
    /// recording that could refuse would be a recording a run could lose.
    pub fn push(
        &self,
        leg: &str,
        dir: Dir,
        at_us: u64,
        wire: impl Into<Vec<u8>>,
        step: Option<&str>,
        note: Option<&str>,
    ) {
        self.push_entry(leg, Entry { dir, at_us, wire: wire.into(), step, note, repeat_of: None });
    }

    /// Record one datagram the caller knows REPEATS an earlier one, resolving
    /// its `repeat_of` back-reference: the earliest datagram on `leg` with the
    /// same bytes in the same direction.
    ///
    /// The caller decides that this is a repeat — the §17.2 seam classified an
    /// arrival, or the step emitted the ladder itself. Resolving WHICH datagram
    /// it repeats is a lookup, and this is where the ladder lives.
    pub fn push_repeat(
        &self,
        leg: &str,
        dir: Dir,
        at_us: u64,
        wire: impl Into<Vec<u8>>,
        step: Option<&str>,
        note: Option<&str>,
    ) {
        let wire = wire.into();
        let repeat_of = self.first_seq_of(leg, dir, &wire);
        self.push_entry(leg, Entry { dir, at_us, wire, step, note, repeat_of });
    }

    /// The `seq` of the earliest datagram on `leg` with these bytes, in this
    /// direction.
    pub fn first_seq_of(&self, leg: &str, dir: Dir, wire: &[u8]) -> Option<u64> {
        let log = self.log.lock().expect("the recording lock outlives its critical sections");
        log.by_leg.get(leg)?.iter().find(|m| m.dir == dir && m.wire() == wire).map(|m| m.seq)
    }

    fn push_entry(&self, leg: &str, entry: Entry<'_>) {
        let mut log =
            self.log.lock().expect("the recording lock is never poisoned by a panic while held");
        let entries = log.by_leg.entry(leg.to_string()).or_default();
        let seq = entries.len() as u64 + 1;
        // The body layout is derived here, from the bytes, whether or not the
        // recorder parsed the datagram: one derivation for every door.
        let mut message = RecordedMessage::new(
            seq,
            entry.dir,
            entry.at_us,
            entry.step.map(str::to_string),
            entry.wire,
        );
        message.repeat_of = entry.repeat_of;
        message.note = entry.note.map(str::to_string);
        entries.push(message);
    }

    /// Attribute an already-recorded datagram to a step, where the step that
    /// claims it is only known after it was recorded (an expect matches the
    /// datagram the receive loop had to record first).
    pub fn attribute(&self, leg: &str, seq: u64, step: &str) {
        let mut log = self.log.lock().expect("the recording lock outlives its critical sections");
        if let Some(entry) =
            log.by_leg.get_mut(leg).and_then(|l| l.iter_mut().find(|m| m.seq == seq))
        {
            entry.step = Some(step.to_string());
        }
    }

    /// Replace the note on one recorded arrival. A HELD datagram is recorded
    /// when it lands, so the bundle keeps wire order and the real instant, and
    /// re-noted once the run knows what became of it.
    pub fn renote(&self, leg: &str, seq: u64, note: &str) {
        let mut log = self.log.lock().expect("the recording lock outlives its critical sections");
        if let Some(entry) =
            log.by_leg.get_mut(leg).and_then(|l| l.iter_mut().find(|m| m.seq == seq))
        {
            entry.note = Some(note.to_string());
        }
    }

    /// The most recently recorded sequence number on `leg`.
    pub fn last_seq(&self, leg: &str) -> Option<u64> {
        let log = self.log.lock().expect("the recording lock outlives its critical sections");
        log.by_leg.get(leg).and_then(|l| l.last()).map(|m| m.seq)
    }

    /// Every leg the run touched, and its messages in wire order.
    pub fn legs(&self) -> BTreeMap<String, Vec<RecordedMessage>> {
        self.log.lock().expect("the recording lock outlives its critical sections").by_leg.clone()
    }

    /// The recording as JSON Lines, one file's worth per leg.
    pub fn to_jsonl(&self) -> BTreeMap<String, String> {
        self.legs()
            .into_iter()
            .map(|(leg, messages)| {
                let text = messages
                    .iter()
                    .map(|m| {
                        pivot_schema::canonical::format_line(m)
                            .expect("a recorded message serializes")
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                (leg, if text.is_empty() { text } else { text + "\n" })
            })
            .collect()
    }

    /// How many datagrams the run recorded, over every leg.
    pub fn len(&self) -> usize {
        self.legs().values().map(Vec::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_leg_s_messages_number_from_one_in_wire_order() {
        let rec = Recording::new();
        rec.push("A", Dir::Out, 0, "INVITE\r\n", Some("s1"), None);
        rec.push("A", Dir::In, 1900, "SIP/2.0 100\r\n", Some("s2"), None);
        rec.push("B", Dir::In, 2000, "INVITE\r\n", None, Some("unclaimed"));
        let legs = rec.legs();
        assert_eq!(legs["A"].iter().map(|m| m.seq).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(legs["B"][0].seq, 1, "each leg numbers its own");
        assert_eq!(rec.len(), 3);
    }

    #[test]
    fn a_datagram_recorded_before_its_step_claimed_it_is_attributed_after_the_fact() {
        let rec = Recording::new();
        rec.push("B", Dir::In, 10, "INVITE\r\n", None, None);
        let seq = rec.last_seq("B").expect("just recorded");
        rec.attribute("B", seq, "s3");
        assert_eq!(rec.legs()["B"][0].step.as_deref(), Some("s3"));
    }

    #[test]
    fn a_declared_leg_that_heard_nothing_still_has_a_ladder() {
        let rec = Recording::new();
        rec.declare("A");
        rec.declare("B");
        rec.push("A", Dir::Out, 0, "INVITE\r\n", Some("s1"), None);
        rec.declare("A");
        let files = rec.to_jsonl();
        assert_eq!(files.len(), 2, "both bound legs have a file");
        assert!(files["B"].is_empty(), "bound and silent is not the same as absent");
        assert_eq!(files["A"].lines().count(), 1, "declaring again disturbs nothing");
    }

    #[test]
    fn a_repeat_points_at_the_earliest_datagram_it_repeats_in_its_own_direction() {
        let rec = Recording::new();
        rec.push("A", Dir::Out, 0, "INVITE\r\n", Some("s1"), None);
        rec.push("A", Dir::In, 1, "INVITE\r\n", None, Some("looped back"));
        rec.push_repeat(
            "A",
            Dir::Out,
            500,
            "INVITE\r\n",
            Some("s1"),
            Some("retransmission 1 of 2"),
        );
        rec.push_repeat(
            "A",
            Dir::Out,
            1500,
            "INVITE\r\n",
            Some("s1"),
            Some("retransmission 2 of 2"),
        );
        let messages = rec.legs()["A"].clone();
        assert_eq!(messages[0].repeat_of, None, "the first is what the others repeat");
        assert_eq!(messages[1].repeat_of, None, "the other direction is another stream");
        assert_eq!(messages[2].repeat_of, Some(1));
        assert_eq!(messages[3].repeat_of, Some(1), "the EARLIEST, not the previous repeat");
    }

    #[test]
    fn the_jsonl_form_is_one_object_per_line_with_a_trailing_newline() {
        let rec = Recording::new();
        rec.push("A", Dir::Out, 0, "INVITE\r\n", Some("s1"), None);
        rec.push("A", Dir::In, 5, "SIP/2.0 100\r\n", None, Some("absorbed repeat"));
        let files = rec.to_jsonl();
        let lines: Vec<&str> = files["A"].lines().collect();
        assert_eq!(lines.len(), 2);
        let first: RecordedMessage = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first.step.as_deref(), Some("s1"));
        assert!(files["A"].ends_with('\n'));
        let second: RecordedMessage = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second.note.as_deref(), Some("absorbed repeat"));
        assert_eq!(second.step, None, "an unclaimed datagram is recorded, not dropped");
    }
}
