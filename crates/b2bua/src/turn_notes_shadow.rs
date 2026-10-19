//! The notes' differential (feature `turn-notes-shadow`): every trace fact and
//! counted metric the direct emission path records is captured on the thread,
//! and each turn's drained notes are compared with what that path recorded
//! over the same core call. Off, every function here does nothing.

use crate::turn_notes::TurnNotes;

#[cfg(feature = "turn-notes-shadow")]
mod on {
    use std::cell::RefCell;
    use std::io::Write;

    /// One thing the direct path recorded.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) enum Recorded {
        Trace { kind: &'static str, at_ms: i64, detail: String },
        Metric { name: &'static str, label: String },
    }

    thread_local! {
        pub(super) static LOG: RefCell<Vec<Recorded>> = const { RefCell::new(Vec::new()) };
        /// How many trace facts, give-ups and caps the notes held at `begin`.
        pub(super) static MARK: RefCell<(usize, usize, u32)> = const { RefCell::new((0, 0, 0)) };
    }

    pub(super) fn push(recorded: Recorded) {
        LOG.with(|log| log.borrow_mut().push(recorded));
    }

    pub(super) fn take() -> Vec<Recorded> {
        LOG.with(|log| std::mem::take(&mut *log.borrow_mut()))
    }

    /// Append one line per compared window to `NOTES_SHADOW_LOG`, when set.
    pub(super) fn tally(compared: usize) {
        let Ok(path) = std::env::var("NOTES_SHADOW_LOG") else { return };
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(file, "{compared}");
        }
    }
}

/// The direct path traced `kind` at `at_ms` with `detail`.
pub fn trace(kind: &'static str, at_ms: i64, detail: &str) {
    #[cfg(feature = "turn-notes-shadow")]
    on::push(on::Recorded::Trace { kind, at_ms, detail: detail.to_string() });
    #[cfg(not(feature = "turn-notes-shadow"))]
    let _ = (kind, at_ms, detail);
}

/// The direct path counted `name` under `label`.
pub fn metric(name: &'static str, label: &str) {
    #[cfg(feature = "turn-notes-shadow")]
    on::push(on::Recorded::Metric { name, label: label.to_string() });
    #[cfg(not(feature = "turn-notes-shadow"))]
    let _ = (name, label);
}

/// A core call starts over `notes`: what the direct path recorded before it,
/// and what the notes held before it, are not its.
pub fn begin(notes: &TurnNotes) {
    #[cfg(feature = "turn-notes-shadow")]
    {
        let _ = on::take();
        let mark = (notes.trace_facts().len(), notes.repeat_give_ups().len(), notes.message_caps());
        on::MARK.with(|m| *m.borrow_mut() = mark);
    }
    #[cfg(not(feature = "turn-notes-shadow"))]
    let _ = notes;
}

/// The core call that began at [`begin`] returned: what it added to `notes`
/// states exactly what the direct path recorded over it, in order.
pub fn check(notes: &TurnNotes) {
    #[cfg(feature = "turn-notes-shadow")]
    {
        let (facts, give_ups, caps) = on::MARK.with(|m| *m.borrow());
        let recorded = on::take();
        let mut drained: Vec<on::Recorded> = notes
            .trace_facts()
            .iter()
            .skip(facts)
            .map(|f| on::Recorded::Trace { kind: f.kind, at_ms: f.at_ms, detail: f.detail.clone() })
            .collect();
        let traced: Vec<on::Recorded> =
            recorded.iter().filter(|r| matches!(r, on::Recorded::Trace { .. })).cloned().collect();
        let mut counted: Vec<on::Recorded> =
            recorded.into_iter().filter(|r| matches!(r, on::Recorded::Metric { .. })).collect();
        let mut noted: Vec<on::Recorded> = notes
            .repeat_give_ups()
            .iter()
            .skip(give_ups)
            .map(|k| on::Recorded::Metric { name: "repeat_give_up", label: (*k).to_string() })
            .collect();
        for _ in caps..notes.message_caps() {
            noted.push(on::Recorded::Metric { name: "message_cap", label: String::new() });
        }
        assert_eq!(drained, traced, "turn-notes differential mismatch: trace");
        counted.sort_by_key(|r| format!("{r:?}"));
        noted.sort_by_key(|r| format!("{r:?}"));
        assert_eq!(noted, counted, "turn-notes differential mismatch: metrics");
        drained.extend(noted);
        on::tally(drained.len());
    }
    #[cfg(not(feature = "turn-notes-shadow"))]
    let _ = notes;
}
