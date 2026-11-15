//! The wire facts the recording stamps on each arrival.

use std::sync::Arc;

use sip_message::SipParser;

/// The wire facts of one arrival, computed ONCE at event production: the
/// lenient parse every downstream reader shares, and the §17.2 wire-view
/// repeat mark from [`crate::repeat`] — the same classification the harness
/// absorption seam runs, so the recorded stream and the TU's surfacing can
/// never disagree on what a repeat is. Stamped for EVERY arrival, audit-visible
/// or not: a repeat of a modeled-loss delivery is still a repeat on the wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WireStamp {
    /// The datagram parsed leniently (the wire is allowed to be malformed —
    /// flagging that is a rule's job, not the parser's); `None` when the bytes
    /// do not parse at all.
    pub parsed: Option<Arc<sip_message::SipMessage>>,
    /// The `seq` of the `RecvItem` this datagram byte-identically repeats
    /// under its §17.2 transaction key, on the same bind.
    pub repeat_of: Option<u64>,
}

impl WireStamp {
    /// Parse `raw` and classify it against `tables` as the arrival stamped
    /// with `seq` — the production path, one call per recorded arrival.
    pub(crate) fn classify(tables: &crate::repeat::RepeatTables, raw: &[u8], seq: u64) -> Self {
        let parsed = crate::rfc_audit::lenient_parser().parse(raw).ok().map(Arc::new);
        let repeat_of = parsed.as_ref().and_then(|m| tables.note(raw, m, seq));
        WireStamp { parsed, repeat_of }
    }

    /// A stamp for a hand-built arrival outside a recording bind (test
    /// fixtures): the lenient parse, and never a repeat.
    pub fn of_bytes(raw: &[u8]) -> Self {
        WireStamp {
            parsed: crate::rfc_audit::lenient_parser().parse(raw).ok().map(Arc::new),
            repeat_of: None,
        }
    }
}
