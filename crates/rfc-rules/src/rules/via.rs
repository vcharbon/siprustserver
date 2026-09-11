//! RFC 3581 — the `rport` bargain struck on the top Via: a sender that asks to
//! be told its source port must be told it, or symmetric-response routing
//! through a NAT has nothing to aim at.
//!
//! **The occasion is a RESPONSE, paired to its request by the top-Via branch.**
//! §4 makes the duty conditional: only a request whose top Via carried a BARE
//! `;rport` asks for anything, so a response is judged only on a branch this
//! vantage saw the same endpoint open with that ask. Everything else opens no
//! occasion at all.
//!
//! **The four states of the parameter are kept apart** ([`sniff::ViaRport`]):
//! absent is the response that dropped the ask, a bare flag or a value no
//! reader accepts is the response that kept the parameter without filling it
//! in, and only an observed port discharges the duty.

use std::collections::BTreeMap;

use sip_message::sniff::{self, ViaRport};

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::{Kind, Msg, WireView};

use super::Obligation;

/// **RFC 3581 §4 — a server that takes `rport` echoes `rport=<source-port>`.**
/// When a request's top Via advertises a bare `;rport`, the response's top Via
/// carries `rport=N` with the source port the server observed. Dropping the
/// parameter, or echoing it empty, leaves the UAC's NAT binding unnamed and the
/// response unroutable back through it.
///
/// The occasion is ONE response the endpoint took on a branch it had opened
/// with a bare `;rport`. Charges the endpoint that sent the response.
///
/// **Advisory by consumer policy**: on a loopback fabric the source-port lookup
/// never triggers — there is no NAT on 127.0.0.1 — so a fake stack that omits
/// the echo is not defective. The finding is surfaced and never gates.
pub struct RportEcho;

impl Obligation for RportEcho {
    fn id(&self) -> RuleId {
        RuleId::RportEcho
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        // Branches on which an endpoint's own SENT request advertised a bare
        // `;rport`, with the method that asked — built as the walk goes, so a
        // response is only ever measured against an ask that preceded it.
        let mut asked: BTreeMap<(&str, &str, &str), &str> = BTreeMap::new();
        let mut out = Vec::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            let Some(branch) = msg.via_branch.as_deref().filter(|b| !b.is_empty()) else {
                continue;
            };
            match &msg.kind {
                Kind::Request { method } => {
                    if rport_of(msg) == ViaRport::Requested {
                        asked.insert(
                            (msg.src.as_str(), msg.call_id.as_str(), branch),
                            method.as_str(),
                        );
                    }
                }
                Kind::Response { status } => {
                    // The ask is the TAKER's: it sent the request, and the
                    // response answers its own top Via.
                    let Some(request_method) =
                        asked.get(&(msg.dst.as_str(), msg.call_id.as_str(), branch)).copied()
                    else {
                        continue;
                    };
                    let finding = |decision| Finding {
                        rule: RuleId::RportEcho,
                        emitter: msg.src.to_string(),
                        taker: msg.dst.to_string(),
                        cseq: msg.cseq,
                        relayed: false,
                        anchor: mi,
                        decision,
                    };
                    if msg.head.is_none() {
                        out.push(finding(Decision::Undecidable("no header block at this vantage")));
                        continue;
                    }
                    let echoed_empty = match rport_of(msg) {
                        ViaRport::Observed(_) => {
                            out.push(finding(Decision::Compliant));
                            continue;
                        }
                        ViaRport::Absent => false,
                        ViaRport::Requested | ViaRport::Unreadable => true,
                    };
                    out.push(finding(Decision::Violated(Evidence::RportNotEchoed {
                        rport_msg: mi,
                        rport_hop: msg.hop,
                        rport_ts_us: msg.at_us,
                        status: *status,
                        request_method: request_method.to_string(),
                        branch: branch.to_string(),
                        echoed_empty,
                    })));
                }
            }
        }
        out
    }
}

/// What the message's top Via says about `rport`. A vantage with no header
/// block states nothing, which reads as no ask and no echo.
fn rport_of(msg: &Msg) -> ViaRport {
    msg.head.as_deref().map_or(ViaRport::Absent, sniff::via_rport)
}

#[cfg(test)]
mod tests {
    //! The rule's OWN semantics under a CLOSED observation: which requests open
    //! an occasion, and which of the four parameter states discharges it.

    use std::collections::BTreeMap;

    use crate::verdict::{Decision, Evidence, Finding};
    use crate::wire::{Kind, Msg, Observation, WireView};

    use super::super::Obligation;
    use super::RportEcho;

    const ALICE: &str = "127.0.0.1:5060";
    const BOB: &str = "127.0.0.1:5070";

    /// An OPTIONS alice sent bob, with a caller-chosen top-Via parameter tail.
    fn ask(at_us: u64, branch: &str, via_tail: &str) -> Msg {
        let head = format!(
            "OPTIONS sip:bob@127.0.0.1 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}{via_tail}\r\n\
             From: <sip:alice@127.0.0.1>;tag=at\r\n\
             To: <sip:bob@127.0.0.1>;tag=bt\r\n\
             Call-ID: c1\r\n\
             CSeq: 5 OPTIONS\r\n\
             Content-Length: 0\r\n\r\n"
        );
        Msg {
            at_us,
            src: ALICE.to_string(),
            dst: BOB.to_string(),
            hop: 0,
            repeat: false,
            kind: Kind::Request { method: "OPTIONS".to_string() },
            call_id: "c1".to_string(),
            cseq: 5,
            cseq_method: "OPTIONS".to_string(),
            from_tag: Some("at".to_string()),
            to_tag: Some("bt".to_string()),
            via_branch: Some(branch.to_string()),
            head: Some(head.into_bytes()),
            body: None,
        }
    }

    /// Bob's 200, echoing whatever the caller chose onto the same branch.
    fn answer(at_us: u64, branch: &str, via_tail: &str) -> Msg {
        let head = format!(
            "SIP/2.0 200 OK\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}{via_tail}\r\n\
             From: <sip:alice@127.0.0.1>;tag=at\r\n\
             To: <sip:bob@127.0.0.1>;tag=bt\r\n\
             Call-ID: c1\r\n\
             CSeq: 5 OPTIONS\r\n\
             Content-Length: 0\r\n\r\n"
        );
        Msg {
            at_us,
            src: BOB.to_string(),
            dst: ALICE.to_string(),
            hop: 0,
            repeat: false,
            kind: Kind::Response { status: 200 },
            call_id: "c1".to_string(),
            cseq: 5,
            cseq_method: "OPTIONS".to_string(),
            from_tag: Some("at".to_string()),
            to_tag: Some("bt".to_string()),
            via_branch: Some(branch.to_string()),
            head: Some(head.into_bytes()),
            body: None,
        }
    }

    fn obs(msgs: &[Msg]) -> Observation {
        let mut endpoint_last_us: BTreeMap<String, u64> = BTreeMap::new();
        let mut last_us = 0;
        for m in msgs {
            last_us = last_us.max(m.at_us);
            for ep in [&m.src, &m.dst] {
                let at = endpoint_last_us.entry(ep.clone()).or_default();
                *at = (*at).max(m.at_us);
            }
        }
        Observation { last_us, endpoint_last_us, closed: true }
    }

    fn eval(msgs: &[Msg]) -> Vec<Finding> {
        RportEcho.eval(&WireView { msgs, obs: &obs(msgs) })
    }

    fn hits(msgs: &[Msg]) -> Vec<Finding> {
        eval(msgs).into_iter().filter(Finding::violated).collect()
    }

    /// The echo carrying the observed port is the obligation met, and it is one
    /// occasion charged to the endpoint that answered.
    #[test]
    fn an_echoed_source_port_is_compliant() {
        let msgs = [ask(1_000, "z9hG4bK-o", ";rport"), answer(2_000, "z9hG4bK-o", ";rport=5060")];
        let all = eval(&msgs);
        assert_eq!(all.len(), 1, "one occasion, the response: {all:?}");
        assert!(matches!(all[0].decision, Decision::Compliant), "{:?}", all[0].decision);
        assert_eq!(all[0].emitter, BOB, "the endpoint that answered is charged");
        assert_eq!(all[0].taker, ALICE);
    }

    /// Dropping the parameter the request advertised is the defect §4 names.
    #[test]
    fn a_dropped_rport_is_violated() {
        let msgs = [ask(1_000, "z9hG4bK-o", ";rport"), answer(2_000, "z9hG4bK-o", "")];
        let f = hits(&msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        let Decision::Violated(Evidence::RportNotEchoed {
            status,
            request_method,
            branch,
            echoed_empty,
            ..
        }) = &f[0].decision
        else {
            panic!("{:?}", f[0].decision)
        };
        assert_eq!((*status, request_method.as_str()), (200, "OPTIONS"));
        assert_eq!(branch.as_str(), "z9hG4bK-o");
        assert!(!echoed_empty, "the parameter is gone, not empty");
    }

    /// Keeping the parameter without filling it in is the other half: a bare
    /// flag and a value no reader accepts both leave the port unnamed.
    #[test]
    fn an_unfilled_rport_is_violated() {
        for tail in [";rport", ";rport=nope"] {
            let msgs = [ask(1_000, "z9hG4bK-o", ";rport"), answer(2_000, "z9hG4bK-o", tail)];
            let f = hits(&msgs);
            assert_eq!(f.len(), 1, "{tail}: {f:?}");
            let Decision::Violated(Evidence::RportNotEchoed { echoed_empty, .. }) = &f[0].decision
            else {
                panic!("{:?}", f[0].decision)
            };
            assert!(echoed_empty, "{tail}");
        }
    }

    /// A request that never asked opens no occasion — §4's duty is conditional
    /// — and neither does a branch this vantage saw no request on.
    #[test]
    fn a_response_to_no_ask_opens_no_occasion() {
        let unasked = [ask(1_000, "z9hG4bK-o", ""), answer(2_000, "z9hG4bK-o", "")];
        assert!(eval(&unasked).is_empty(), "{:?}", eval(&unasked));

        let orphan = [answer(1_000, "z9hG4bK-unseen", "")];
        assert!(eval(&orphan).is_empty());
    }

    /// The ask is the TAKER's own: a response answering someone else's branch
    /// on the same wire is not this endpoint's occasion.
    #[test]
    fn the_ask_belongs_to_the_endpoint_that_sent_the_request() {
        let mut elsewhere = ask(1_000, "z9hG4bK-o", ";rport");
        elsewhere.src = BOB.to_string();
        elsewhere.dst = ALICE.to_string();
        let msgs = [elsewhere, answer(2_000, "z9hG4bK-o", "")];
        assert!(eval(&msgs).is_empty(), "bob asked, so bob's own response is not judged here");
    }
}
