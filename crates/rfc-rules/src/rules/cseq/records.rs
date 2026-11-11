//! The rows the CSeq walk collects: each new request, ACK and response.

use sip_message::header::same_branch;
use sip_message::sniff::OriginVia;

use crate::verdict::Evidence;

use super::ForwardedKey;

/// One new in-dialog request as the taker accounted it.
#[derive(Debug)]
pub(super) struct Request<'a> {
    pub(super) msg: usize,
    pub(super) hop: usize,
    pub(super) ts_us: u64,
    pub(super) cseq: u32,
    /// The request-line method, as the wire spelled it.
    pub(super) method: &'a str,
    /// The top-Via branch, `""` where the vantage carried none.
    pub(super) branch: &'a str,
    pub(super) emitter: &'a str,
    /// The socket it reached.
    pub(super) taker: &'a str,
    pub(super) from_tag: &'a str,
    /// The request's bottom Via, where the vantage carried its head: the hop
    /// that originated it.
    pub(super) origin: Option<OriginVia>,
    /// The request as a forwarder keeps it, where a bottom Via names it.
    pub(super) forwarded: Option<ForwardedKey<'a>>,
}

impl Request<'_> {
    /// Whether `copy`, a later request on this one's number, is
    /// its originator's own reuse rather than this request by another path or
    /// another originator's — read off the two bottom Vias (see the module
    /// doc). Without both Vias, or a bottom branch, the reuse reading stands.
    pub(super) fn reused_by(&self, copy: &Request<'_>) -> bool {
        let (Some(first), Some(again)) = (&self.origin, &copy.origin) else { return true };
        if !first.sent_by.as_borrowed().same_host(&again.sent_by.as_borrowed()) {
            return false;
        }
        let same = first.branch.as_deref().zip(again.branch.as_deref());
        first.branch.is_none() || !same.is_some_and(|(a, b)| same_branch(a, b))
    }

    pub(super) fn not_contiguous(&self, prior_cseq: u32, to_tag: &str) -> Evidence {
        Evidence::CseqNotContiguous {
            skip_msg: self.msg,
            skip_hop: self.hop,
            skip_ts_us: self.ts_us,
            cseq: self.cseq,
            prior_cseq,
            method: self.method.to_string(),
            from_tag: self.from_tag.to_string(),
            to_tag: to_tag.to_string(),
        }
    }
}

/// One ACK the stream carried, with the INVITE numbers known when it arrived.
#[derive(Debug)]
pub(super) struct Ack<'a> {
    pub(super) msg: usize,
    pub(super) hop: usize,
    pub(super) ts_us: u64,
    pub(super) cseq: u32,
    pub(super) emitter: &'a str,
    /// The socket it reached.
    pub(super) taker: &'a str,
    /// `None` where the stream had carried no INVITE yet — nothing to judge
    /// the ACK against.
    pub(super) known: Option<Vec<u32>>,
}

/// One response the view carried.
#[derive(Debug)]
pub(super) struct Response<'a> {
    pub(super) msg: usize,
    pub(super) hop: usize,
    pub(super) ts_us: u64,
    pub(super) status: u16,
    pub(super) cseq: u32,
    /// The CSeq header's method, as the wire spelled it.
    pub(super) method: &'a str,
    pub(super) branch: Option<&'a str>,
    pub(super) emitter: &'a str,
    pub(super) taker: &'a str,
}
