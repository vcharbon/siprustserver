//! The fields of one datagram every per-call watcher reads, sniffed once per
//! datagram and shared: the round-trip tracker and the message tally.

use sip_message::sniff::{
    call_id, cseq_method_token, cseq_number, req_method, resp_status, rseq_of,
};

/// One datagram's identity fields (`None` where the datagram lacks one).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Sniffed {
    pub(crate) call_id: Option<String>,
    pub(crate) cseq: Option<u32>,
    /// The CSeq method token.
    pub(crate) cseq_method: Option<String>,
    /// The status of a response.
    pub(crate) status: Option<u16>,
    /// The method of a request.
    pub(crate) method: Option<String>,
    /// The RSeq of a provisional response.
    pub(crate) rseq: Option<u64>,
}

impl Sniffed {
    pub(crate) fn of(raw: &[u8]) -> Self {
        let status = resp_status(raw);
        Self {
            call_id: call_id(raw),
            cseq: cseq_number(raw),
            cseq_method: cseq_method_token(raw),
            method: if status.is_some() { None } else { req_method(raw) },
            rseq: status.filter(|s| (100..200).contains(s)).and_then(|_| rseq_of(raw)),
            status,
        }
    }

    /// The message as a ladder labels it: the request method (`INVITE`), or
    /// the status and the CSeq method (`180 (INVITE)`); `?` when unreadable.
    pub(crate) fn label(&self) -> String {
        match (self.status, &self.method, &self.cseq_method) {
            (Some(status), _, Some(m)) => format!("{status} ({m})"),
            (Some(status), _, None) => status.to_string(),
            (None, Some(method), _) => method.clone(),
            (None, None, _) => "?".to_string(),
        }
    }
}
