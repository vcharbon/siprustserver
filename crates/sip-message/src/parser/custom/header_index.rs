//! The one dispatch pass over a parsed header list.
//!
//! Every header the eager field model reads — mandatory and optional alike — is
//! located in a single walk that dispatches on the [`HeaderId`] each line was
//! classified as. Extraction then reads slots instead of re-scanning the list
//! per header type, so the cost of a parse is O(headers), not O(header types ×
//! headers).
//!
//! Values are borrowed from the header list and absent headers own nothing, so
//! indexing a message allocates only for the multi-valued headers it actually
//! carries.

use crate::header::HeaderId;
use crate::header_table::HeaderTable;
use crate::sip_str::SipStr;
use crate::types::SipHeader;

/// A single-valued header's one line: every way a message is made refuses a
/// second.
#[derive(Debug, Default, Clone, Copy)]
pub struct Slot<'a> {
    pub value: Option<&'a SipStr>,
}

impl<'a> Slot<'a> {
    fn record(&mut self, value: &'a SipStr) {
        self.value.get_or_insert(value);
    }
}

/// The located header values of one message.
#[derive(Debug, Default)]
pub struct HeaderIndex<'a> {
    pub from: Slot<'a>,
    pub to: Slot<'a>,
    pub call_id: Slot<'a>,
    pub cseq: Slot<'a>,
    pub via: Vec<&'a SipStr>,
    pub contact: Vec<&'a SipStr>,
    pub route: Vec<&'a SipStr>,
    pub record_route: Vec<&'a SipStr>,
    pub p_asserted_identity: Vec<&'a SipStr>,
    pub p_preferred_identity: Vec<&'a SipStr>,
    pub diversion: Vec<&'a SipStr>,
    pub history_info: Vec<&'a SipStr>,
    pub remote_party_id: Vec<&'a SipStr>,
    pub rack: Slot<'a>,
    pub refer_to: Slot<'a>,
    pub max_forwards: Slot<'a>,
}

impl<'a> HeaderIndex<'a> {
    /// Walk the classified lines of `headers` once, filing each value under
    /// its header. Headers the field model does not read are skipped.
    pub fn build(headers: &'a HeaderTable) -> Self {
        Self::file(headers.classified_lines())
    }

    /// [`build`](Self::build) over lines no table classified yet: each is
    /// classified here, once; compact forms file under their long name
    /// (RFC 3261 §7.3.3).
    pub fn of_lines(headers: &'a [SipHeader]) -> Self {
        Self::file(headers.iter().map(|h| (HeaderId::of(&h.name), h)))
    }

    fn file(lines: impl Iterator<Item = (Option<HeaderId>, &'a SipHeader)>) -> Self {
        let mut idx = Self::default();
        for (id, header) in lines {
            let Some(id) = id else { continue };
            let value = &header.value;
            match id {
                HeaderId::From => idx.from.record(value),
                HeaderId::To => idx.to.record(value),
                HeaderId::CallId => idx.call_id.record(value),
                HeaderId::CSeq => idx.cseq.record(value),
                HeaderId::Via => idx.via.push(value),
                HeaderId::Contact => idx.contact.push(value),
                HeaderId::Route => idx.route.push(value),
                HeaderId::RecordRoute => idx.record_route.push(value),
                HeaderId::PAssertedIdentity => idx.p_asserted_identity.push(value),
                HeaderId::PPreferredIdentity => idx.p_preferred_identity.push(value),
                HeaderId::Diversion => idx.diversion.push(value),
                HeaderId::HistoryInfo => idx.history_info.push(value),
                HeaderId::RemotePartyId => idx.remote_party_id.push(value),
                HeaderId::RAck => idx.rack.record(value),
                HeaderId::ReferTo => idx.refer_to.record(value),
                HeaderId::MaxForwards => idx.max_forwards.record(value),
                _ => {}
            }
        }
        idx
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(name: &str, value: &str) -> SipHeader {
        SipHeader::new(SipStr::owned(name), SipStr::owned(value))
    }

    #[test]
    fn a_single_walk_files_every_read_header() {
        let headers = vec![
            header("Via", "SIP/2.0/UDP a"),
            header("v", "SIP/2.0/UDP b"),
            header("From", "<sip:a@h>;tag=1"),
            header("t", "<sip:b@h>"),
            header("Call-ID", "cid"),
            header("CSeq", "1 INVITE"),
            header("X-Custom", "ignored"),
        ];
        let idx = HeaderIndex::of_lines(&headers);
        assert_eq!(idx.via.len(), 2, "compact Via files under Via");
        assert_eq!(idx.to.value.map(|v| v.as_str()), Some("<sip:b@h>"));
        assert!(idx.from.value.is_some());
        assert_eq!(idx.call_id.value.map(|v| v.as_str()), Some("cid"));
        assert!(idx.contact.is_empty());
    }
}
