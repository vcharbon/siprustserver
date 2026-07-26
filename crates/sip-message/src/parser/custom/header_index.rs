//! The one dispatch pass over a parsed header list.
//!
//! Every header the eager field model reads — mandatory and optional alike — is
//! located in a single walk that dispatches on [`HeaderName`]. Extraction then
//! reads slots instead of re-scanning the list per header type, so the cost of
//! a parse is O(headers), not O(header types × headers).
//!
//! Values are borrowed from the header list and absent headers own nothing, so
//! indexing a message allocates only for the multi-valued headers it actually
//! carries.

use crate::header::HeaderName;
use crate::sip_str::SipStr;
use crate::types::SipHeader;

/// A header RFC 3261 allows at most once. `first` is the value every reader
/// uses; `count` is what the cardinality gates check.
#[derive(Debug, Default, Clone, Copy)]
pub struct Slot<'a> {
    pub first: Option<&'a SipStr>,
    pub count: usize,
}

impl<'a> Slot<'a> {
    fn record(&mut self, value: &'a SipStr) {
        self.count += 1;
        if self.first.is_none() {
            self.first = Some(value);
        }
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
    pub p_asserted_identity: Vec<&'a SipStr>,
    pub p_preferred_identity: Vec<&'a SipStr>,
    pub diversion: Vec<&'a SipStr>,
    pub history_info: Vec<&'a SipStr>,
    pub remote_party_id: Vec<&'a SipStr>,
    pub geolocation: Vec<&'a SipStr>,
    pub geolocation_error: Vec<&'a SipStr>,
    pub geolocation_routing: Slot<'a>,
    pub rack: Slot<'a>,
    pub refer_to: Slot<'a>,
    pub date: Vec<&'a SipStr>,
}

impl<'a> HeaderIndex<'a> {
    /// Walk `headers` once, filing each value under its name. Names the field
    /// model does not read are skipped; compact forms file under their long
    /// name (RFC 3261 §7.3.3).
    pub fn build(headers: &'a [SipHeader]) -> Self {
        let mut idx = Self::default();
        for header in headers {
            let Some(name) = HeaderName::known(&header.name) else { continue };
            let value = &header.value;
            match name {
                HeaderName::From => idx.from.record(value),
                HeaderName::To => idx.to.record(value),
                HeaderName::CallId => idx.call_id.record(value),
                HeaderName::CSeq => idx.cseq.record(value),
                HeaderName::Via => idx.via.push(value),
                HeaderName::Contact => idx.contact.push(value),
                HeaderName::PAssertedIdentity => idx.p_asserted_identity.push(value),
                HeaderName::PPreferredIdentity => idx.p_preferred_identity.push(value),
                HeaderName::Diversion => idx.diversion.push(value),
                HeaderName::HistoryInfo => idx.history_info.push(value),
                HeaderName::RemotePartyId => idx.remote_party_id.push(value),
                HeaderName::Geolocation => idx.geolocation.push(value),
                HeaderName::GeolocationError => idx.geolocation_error.push(value),
                HeaderName::GeolocationRouting => idx.geolocation_routing.record(value),
                HeaderName::RAck => idx.rack.record(value),
                HeaderName::ReferTo => idx.refer_to.record(value),
                HeaderName::Date => idx.date.push(value),
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
        let idx = HeaderIndex::build(&headers);
        assert_eq!(idx.via.len(), 2, "compact Via files under Via");
        assert_eq!(idx.to.first.map(|v| v.as_str()), Some("<sip:b@h>"));
        assert_eq!(idx.from.count, 1);
        assert_eq!(idx.call_id.first.map(|v| v.as_str()), Some("cid"));
        assert!(idx.contact.is_empty());
    }

    #[test]
    fn a_repeated_single_valued_header_keeps_the_first_and_counts_both() {
        let headers = vec![header("From", "<sip:a@h>"), header("From", "<sip:b@h>")];
        let idx = HeaderIndex::build(&headers);
        assert_eq!(idx.from.count, 2);
        assert_eq!(idx.from.first.map(|v| v.as_str()), Some("<sip:a@h>"));
    }
}
