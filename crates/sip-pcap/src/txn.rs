//! Transactions of a leg: a request joined to its responses, derived on
//! demand from the leg's messages. Query-time only — the emitted model stays
//! message-shaped, and this view is rebuilt by whoever needs the join.
//!
//! It exists because the interesting questions about a call are transactional,
//! not per-message: "was the UPDATE rejected", "did the re-INVITE's 200 carry
//! this SDP", "how long before that INVITE was refused". None of those is
//! expressible over independent message predicates — each is a request shape
//! ANDed with a response shape, scoped to one transaction.
//!
//! Keyed by (top-Via branch, CSeq method), RFC 3261 §17. A relayed leg observes
//! the same transaction at several hops, so a transaction holds every
//! observation; the timing fields use the FIRST of each, which is the one
//! nearest the sender.

use sip_message::{Method, SipMessage};

use crate::flow::FlowLeg;

/// Where a transaction sits in its dialog — the axis a query filters on when
/// it means "the re-INVITE" rather than "an INVITE".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnKind {
    /// The INVITE that created the dialog.
    InitialInvite,
    /// A later INVITE on an established dialog.
    ReInvite,
    /// A non-INVITE request inside the dialog (its To carries a tag).
    InDialog,
    /// A request outside any dialog (OPTIONS, an out-of-dialog REFER, …).
    OutOfDialog,
}

impl TxnKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            TxnKind::InitialInvite => "initial_invite",
            TxnKind::ReInvite => "reinvite",
            TxnKind::InDialog => "in_dialog",
            TxnKind::OutOfDialog => "out_of_dialog",
        }
    }

    /// Parse the wire spelling used in queries; `None` for an unknown name.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "initial_invite" => Some(TxnKind::InitialInvite),
            "reinvite" => Some(TxnKind::ReInvite),
            "in_dialog" => Some(TxnKind::InDialog),
            "out_of_dialog" => Some(TxnKind::OutOfDialog),
            _ => None,
        }
    }
}

/// One transaction of a leg. Message fields are indices into
/// [`FlowLeg::msgs`], so a consumer reaches the exact wire bytes.
#[derive(Debug, Clone)]
pub struct Txn {
    pub branch: String,
    pub method: Method,
    pub cseq: u32,
    pub kind: TxnKind,
    /// First observation of the request — absent when the capture missed it
    /// (a vantage that saw only the responses).
    pub request: Option<usize>,
    /// Every response observation, in capture order.
    pub responses: Vec<usize>,
    /// Status of the first final (>=200) response.
    pub final_status: Option<u16>,
    /// Request → first final response. `None` when either end is uncaptured —
    /// never zero, so "took longer than X" cannot silently match a gap.
    pub latency_us: Option<u64>,
}

impl Txn {
    /// Statuses of the provisional (1xx) responses, in capture order.
    pub fn provisionals<'a>(&'a self, leg: &'a FlowLeg) -> impl Iterator<Item = u16> + 'a {
        self.responses.iter().filter_map(move |&i| match &leg.msgs[i].parsed {
            SipMessage::Response(r) if r.status() < 200 => Some(r.status()),
            _ => None,
        })
    }

    /// The first final (>=200) response observation.
    pub fn final_response(&self, leg: &FlowLeg) -> Option<usize> {
        self.responses.iter().copied().find(|&i| {
            matches!(&leg.msgs[i].parsed, SipMessage::Response(r) if r.status() >= 200)
        })
    }
}

/// Derive every transaction of a leg, ordered by first observation.
pub fn transactions(leg: &FlowLeg) -> Vec<Txn> {
    let initial_cseq = leg.invite.as_ref().map(|inv| inv.cseq);
    let mut txns: Vec<Txn> = Vec::new();
    // (branch, cseq method) → index, the RFC 3261 §17 transaction key.
    let mut index: Vec<(String, Method, usize)> = Vec::new();

    for (mi, msg) in leg.msgs.iter().enumerate() {
        let (branch, method, cseq, is_request) = match &msg.parsed {
            SipMessage::Request(r) => {
                (branch_of(&msg.parsed), r.cseq().method().clone(), r.cseq().seq(), true)
            }
            SipMessage::Response(r) => {
                (branch_of(&msg.parsed), r.cseq().method().clone(), r.cseq().seq(), false)
            }
        };
        let slot = index
            .iter()
            .find(|(b, m, _)| b == &branch && m == method)
            .map(|(_, _, i)| *i);
        let ti = match slot {
            Some(i) => i,
            None => {
                txns.push(Txn {
                    branch: branch.clone(),
                    method: method.clone(),
                    cseq,
                    kind: classify(msg, &method, cseq, initial_cseq),
                    request: None,
                    responses: Vec::new(),
                    final_status: None,
                    latency_us: None,
                });
                index.push((branch, method.clone(), txns.len() - 1));
                txns.len() - 1
            }
        };
        let txn = &mut txns[ti];
        if is_request {
            if txn.request.is_none() {
                txn.request = Some(mi);
                // A request observed after its response (a later hop) must not
                // yield a negative or bogus latency.
                txn.kind = classify(msg, &method, cseq, initial_cseq);
            }
        } else {
            txn.responses.push(mi);
            if let SipMessage::Response(r) = &msg.parsed {
                if r.status() >= 200 && txn.final_status.is_none() {
                    txn.final_status = Some(r.status());
                    txn.latency_us = txn
                        .request
                        .map(|ri| msg.ts_us.saturating_sub(leg.msgs[ri].ts_us));
                }
            }
        }
    }
    txns
}

/// The branch of a message's top Via, or the empty string when it carries none
/// — an unbranched Via cannot key a transaction, so those messages share one
/// bucket per method rather than being dropped.
fn branch_of(msg: &SipMessage) -> String {
    msg.top_via().branch().unwrap_or_default().to_string()
}

/// A request's place in its dialog. A response never reclassifies a
/// transaction — only the request carries the To-tag that distinguishes an
/// in-dialog request from an out-of-dialog one.
fn classify(
    msg: &crate::flow::FlowMsg,
    method: &Method,
    cseq: u32,
    initial_cseq: Option<u32>,
) -> TxnKind {
    let SipMessage::Request(r) = &msg.parsed else {
        return if *method == Method::Invite && initial_cseq == Some(cseq) {
            TxnKind::InitialInvite
        } else {
            TxnKind::OutOfDialog
        };
    };
    if *method == Method::Invite {
        // The dialog-creating INVITE carries no To-tag; everything else on the
        // same leg is a re-INVITE.
        return match (r.to().tag().is_none(), initial_cseq == Some(cseq)) {
            (true, _) | (_, true) => TxnKind::InitialInvite,
            _ => TxnKind::ReInvite,
        };
    }
    if r.to().tag().is_some() {
        TxnKind::InDialog
    } else {
        TxnKind::OutOfDialog
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flow::{build_flows, FlowConfig};
    use crate::Datagram;

    fn dg(ts_us: u64, src: &str, dst: &str, payload: &[u8]) -> Datagram {
        Datagram {
            ts_us,
            src: src.parse().unwrap(),
            dst: dst.parse().unwrap(),
            payload: payload.to_vec(),
        probe: 0,
        }
    }

    fn req(method: &str, cseq: u32, branch: &str, to_tag: &str, extra: &str) -> Vec<u8> {
        format!(
            "{method} sip:bob@10.0.0.9 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK{branch}\r\n\
             Max-Forwards: 70\r\n\
             From: <sip:alice@10.0.0.1>;tag=f1\r\n\
             To: <sip:bob@10.0.0.9>{to_tag}\r\n\
             Call-ID: txn-1\r\n\
             CSeq: {cseq} {method}\r\n\
             {extra}\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    fn resp(status: u16, cseq: u32, method: &str, branch: &str, body: &str) -> Vec<u8> {
        format!(
            "SIP/2.0 {status} X\r\n\
             Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK{branch}\r\n\
             From: <sip:alice@10.0.0.1>;tag=f1\r\n\
             To: <sip:bob@10.0.0.9>;tag=t1\r\n\
             Call-ID: txn-1\r\n\
             CSeq: {cseq} {method}\r\n\
             {}\
             Content-Length: {}\r\n\r\n{body}",
            if body.is_empty() { "" } else { "Content-Type: application/sdp\r\n" },
            body.len()
        )
        .into_bytes()
    }

    fn one_leg(datagrams: Vec<Datagram>) -> (crate::flow::Flows, Vec<Txn>) {
        let flows = build_flows(&datagrams, &FlowConfig::default());
        let txns = transactions(&flows.legs[0]);
        (flows, txns)
    }

    /// A call with a rejected UPDATE and a re-INVITE answered 200: each
    /// request lands in its own transaction, joined to its own responses.
    #[test]
    fn requests_join_their_own_responses() {
        let datagrams = vec![
            dg(1_000_000, "10.0.0.1:5060", "10.0.0.9:5060", &req("INVITE", 1, "b1", "", "")),
            dg(1_100_000, "10.0.0.9:5060", "10.0.0.1:5060", &resp(180, 1, "INVITE", "b1", "")),
            dg(2_000_000, "10.0.0.9:5060", "10.0.0.1:5060", &resp(200, 1, "INVITE", "b1", "")),
            dg(3_000_000, "10.0.0.1:5060", "10.0.0.9:5060", &req("UPDATE", 2, "b2", ";tag=t1", "")),
            dg(3_500_000, "10.0.0.9:5060", "10.0.0.1:5060", &resp(488, 2, "UPDATE", "b2", "")),
            dg(4_000_000, "10.0.0.1:5060", "10.0.0.9:5060", &req("INVITE", 3, "b3", ";tag=t1", "")),
            dg(4_200_000, "10.0.0.9:5060", "10.0.0.1:5060", &resp(200, 3, "INVITE", "b3", "a=sendonly")),
        ];
        let (flows, txns) = one_leg(datagrams);
        let leg = &flows.legs[0];
        assert_eq!(txns.len(), 3, "{txns:#?}");

        assert_eq!(txns[0].kind, TxnKind::InitialInvite);
        assert_eq!(txns[0].final_status, Some(200));
        assert_eq!(txns[0].latency_us, Some(1_000_000));
        assert_eq!(txns[0].provisionals(leg).collect::<Vec<_>>(), vec![180]);

        assert_eq!(txns[1].method, Method::Update);
        assert_eq!(txns[1].kind, TxnKind::InDialog);
        assert_eq!(txns[1].final_status, Some(488), "the rejected UPDATE");
        assert_eq!(txns[1].latency_us, Some(500_000));

        assert_eq!(txns[2].kind, TxnKind::ReInvite);
        assert_eq!(txns[2].final_status, Some(200));
        // The re-INVITE's own 200 is the one carrying the SDP.
        let fr = txns[2].final_response(leg).expect("final response captured");
        assert!(String::from_utf8_lossy(leg.msgs[fr].raw()).contains("a=sendonly"));
    }

    /// The same transaction observed at two hops stays ONE transaction, and
    /// its latency is measured from the first observation of each end.
    #[test]
    fn multi_hop_observations_do_not_split_a_transaction() {
        let invite = req("INVITE", 1, "b1", "", "");
        let ok = resp(200, 1, "INVITE", "b1", "");
        let datagrams = vec![
            dg(1_000_000, "10.0.0.1:5060", "10.0.0.5:5060", &invite),
            dg(1_010_000, "10.0.0.5:5061", "10.0.0.9:5060", &invite),
            dg(2_000_000, "10.0.0.9:5060", "10.0.0.5:5061", &ok),
            dg(2_010_000, "10.0.0.5:5060", "10.0.0.1:5060", &ok),
        ];
        let (_, txns) = one_leg(datagrams);
        assert_eq!(txns.len(), 1);
        assert_eq!(txns[0].responses.len(), 2, "both vantages kept");
        assert_eq!(txns[0].latency_us, Some(1_000_000));
    }

    /// A vantage that saw only the responses yields a transaction with no
    /// request and NO latency — never a zero that would satisfy "took under X".
    #[test]
    fn an_uncaptured_request_yields_no_latency() {
        let datagrams = vec![dg(
            2_000_000,
            "10.0.0.9:5060",
            "10.0.0.1:5060",
            &resp(200, 1, "INVITE", "b1", ""),
        )];
        let (_, txns) = one_leg(datagrams);
        assert_eq!(txns.len(), 1);
        assert!(txns[0].request.is_none());
        assert_eq!(txns[0].final_status, Some(200));
        assert_eq!(txns[0].latency_us, None);
    }
}
