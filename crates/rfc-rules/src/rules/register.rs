//! RFC 3261 §10.2 — what a UA registering a binding may say, and how fast it
//! may say the next thing. TWO obligations, both charging the REGISTER's
//! sender:
//!
//!   - [`RegisterNoRouteSet`] — a REGISTER states no Route set. Registration is
//!     not a dialog and forms none, so a Route header on it routes a binding
//!     request down a path nothing agreed.
//!   - [`SerialRegister`] — a UA changes the Contact of an address-of-record
//!     only once the previous REGISTER for it has been answered. Two in flight
//!     with different Contacts leave the registrar's binding set decided by
//!     arrival order.
//!
//! **The occasion is one REGISTER an endpoint SENT**, fresh (a retransmission
//! is the same act again, never a second binding attempt). Both obligations are
//! read off the message's own header block, so a vantage that carried none
//! settles neither: the occasion stands and its verdict is `Undecidable`.

use std::collections::BTreeMap;

use sip_message::sniff;

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::WireView;

use super::branch::{BranchKey, BranchReading};
use super::Obligation;

/// **RFC 3261 §10.2 — a REGISTER carries no Route header.** Registration
/// creates no dialog and so has no route set to reproduce; a Route row on one
/// asks an outbound proxy to forward a binding request along a path the
/// registration never established.
///
/// The occasion is one REGISTER sent; carrying no Route discharges it. Charges
/// the sender.
pub struct RegisterNoRouteSet;

impl Obligation for RegisterNoRouteSet {
    fn id(&self) -> RuleId {
        RuleId::RegisterNoRouteSet
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let mut out = Vec::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            if msg.repeat || !msg.is_request("REGISTER") {
                continue;
            }
            let finding = |decision| Finding {
                rule: RuleId::RegisterNoRouteSet,
                emitter: msg.src.to_string(),
                taker: msg.dst.to_string(),
                cseq: msg.cseq,
                relayed: false,
                anchor: mi,
                decision,
            };
            let Some(head) = msg.head.as_deref() else {
                out.push(finding(Decision::Undecidable("no header block at this vantage")));
                continue;
            };
            let routes = sniff::header_values(head, "route");
            if routes.is_empty() {
                out.push(finding(Decision::Compliant));
                continue;
            }
            out.push(finding(Decision::Violated(Evidence::RegisterCarriesRoute {
                register_route_msg: mi,
                register_route_hop: msg.hop,
                register_route_ts_us: msg.at_us,
                routes,
            })));
        }
        out
    }
}

/// **RFC 3261 §10.2 — one binding change at a time.** A UA does not send a
/// REGISTER whose Contact differs from a REGISTER it has outstanding for the
/// same address-of-record: the two race inside the registrar and which binding
/// survives is decided by arrival order rather than by the UA.
///
/// **The occasion is one REGISTER sent whose AOR is readable**, and the AOR is
/// the To URI (§10.2 names the record being bound there, never in the From).
/// The obligation is met when nothing was outstanding for that AOR, when the
/// outstanding one carried the same Contact, or when it had already been
/// answered.
///
/// **"Answered" is answered BEFORE this REGISTER left.** The final that
/// discharges the previous transaction is read at its own place in the view: a
/// response that arrives afterwards did not license a send that had already
/// happened.
///
/// **A REGISTER with no top-Via branch is no occasion.** The branch is how the
/// outstanding transaction's answer is found; without one, nothing can say
/// whether it is still in flight.
///
/// Charges the sender.
pub struct SerialRegister;

impl Obligation for SerialRegister {
    fn id(&self) -> RuleId {
        RuleId::SerialRegister
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = BranchReading::of(wire.msgs);
        // (sender, AOR) → the REGISTER still outstanding for that binding.
        let mut pending: BTreeMap<(&str, String), Pending<'_>> = BTreeMap::new();
        let mut out = Vec::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            if msg.repeat || !msg.is_request("REGISTER") {
                continue;
            }
            let Some(branch) = msg.via_branch.as_deref().filter(|b| !b.is_empty()) else {
                continue;
            };
            let sender = msg.src.as_str();
            let finding = |decision| Finding {
                rule: RuleId::SerialRegister,
                emitter: msg.src.to_string(),
                taker: msg.dst.to_string(),
                cseq: msg.cseq,
                relayed: false,
                anchor: mi,
                decision,
            };
            let Some(head) = msg.head.as_deref() else {
                out.push(finding(Decision::Undecidable("no header block at this vantage")));
                continue;
            };
            let Some(aor) = sniff::name_addr_uri(head, "to") else {
                out.push(finding(Decision::Undecidable(
                    "the REGISTER names no readable address-of-record",
                )));
                continue;
            };
            // The Contact ROWS as the sender wrote them: what the registrar is
            // being asked to bind, compared row for row.
            let contact = sniff::header_values(head, "contact").join(",");
            let key = (sender, aor.clone());
            if pending.get(&key).is_some_and(|p| p.answered_before(mi, &seen)) {
                pending.remove(&key);
            }
            match pending.get(&key) {
                Some(prior) if prior.contact != contact => {
                    out.push(finding(Decision::Violated(Evidence::ConcurrentRegister {
                        concurrent_register_msg: mi,
                        concurrent_register_hop: msg.hop,
                        concurrent_register_ts_us: msg.at_us,
                        aor,
                        contact,
                        pending_contact: prior.contact.clone(),
                        branch: branch.to_string(),
                        pending_branch: prior.key.branch.to_string(),
                        pending_msg: prior.msg,
                    })));
                }
                // Re-registering the SAME Contact is a retry of one binding
                // attempt, not a second one: the first stays outstanding.
                Some(_) => out.push(finding(Decision::Compliant)),
                None => {
                    pending.insert(
                        key,
                        Pending { key: BranchKey::sent_by(msg, branch), contact, msg: mi },
                    );
                    out.push(finding(Decision::Compliant));
                }
            }
        }
        out
    }
}

/// The REGISTER a sender has outstanding for one address-of-record.
#[derive(Debug)]
struct Pending<'a> {
    /// The transaction its answer will come back on.
    key: BranchKey<'a>,
    /// The Contact rows it asked the registrar to bind.
    contact: String,
    /// Index into the view's `msgs`.
    msg: usize,
}

impl<'a> Pending<'a> {
    /// Whether a final answered this REGISTER before view index `before`.
    fn answered_before(&self, before: usize, seen: &BranchReading<'a>) -> bool {
        seen.at(&self.key).and_then(|b| b.first_final).is_some_and(|at| at < before)
    }
}

#[cfg(test)]
mod tests {
    //! The family's OWN semantics: which REGISTER is an occasion, what the
    //! header block settles, and WHEN an answer discharges an outstanding
    //! binding attempt.

    use std::collections::BTreeMap;

    use crate::verdict::{Decision, Evidence, Finding, RuleId};
    use crate::wire::{Kind, Msg, Observation, WireView};

    use super::super::Obligation;
    use super::{RegisterNoRouteSet, SerialRegister};

    const UAC: &str = "10.0.0.1:5060";
    const REGISTRAR: &str = "10.0.0.9:5060";

    /// A REGISTER the UA SENT on `branch`, binding `contact` to the AOR, with
    /// `extra` header rows appended.
    fn register(at_us: u64, branch: &str, contact: &str, extra: &str) -> Msg {
        let head = format!(
            "REGISTER sip:h SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.1:5060;branch={branch}\r\n\
             From: <sip:alice@h>;tag=at\r\n\
             To: <sip:alice@h>\r\n\
             Call-ID: c1\r\n\
             CSeq: 1 REGISTER\r\n\
             Contact: {contact}\r\n\
             {extra}\r\n"
        );
        Msg {
            at_us,
            src: UAC.to_string(),
            dst: REGISTRAR.to_string(),
            hop: 0,
            repeat: false,
            kind: Kind::Request { method: "REGISTER".to_string() },
            call_id: "c1".to_string(),
            cseq: 1,
            cseq_method: "REGISTER".to_string(),
            via_branch: Some(branch.to_string()),
            from_tag: Some("at".to_string()),
            to_tag: None,
            head: Some(head.into_bytes()),
            body: None,
        }
    }

    /// The registrar's final on `branch`.
    fn answered(at_us: u64, branch: &str, status: u16) -> Msg {
        let mut m = register(at_us, branch, "<sip:a@1>", "");
        m.src = REGISTRAR.to_string();
        m.dst = UAC.to_string();
        m.kind = Kind::Response { status };
        m.to_tag = Some("rt".to_string());
        m
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

    fn routeless(msgs: &[Msg]) -> Vec<Finding> {
        RegisterNoRouteSet.eval(&WireView { msgs, obs: &obs(msgs) })
    }

    fn serial(msgs: &[Msg]) -> Vec<Finding> {
        SerialRegister.eval(&WireView { msgs, obs: &obs(msgs) })
    }

    /// A REGISTER stating no path is the met occasion.
    #[test]
    fn a_routeless_register_is_compliant() {
        let f = routeless(&[register(1_000, "z9hG4bK-1", "<sip:a@1>", "")]);
        assert_eq!(f.len(), 1, "one occasion, the REGISTER: {f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
        assert_eq!(f[0].emitter, UAC, "the sender is charged");
        assert_eq!(f[0].taker, REGISTRAR);
    }

    /// The violation: registration forms no route set, so a Route row on one
    /// asks for a path nothing agreed.
    #[test]
    fn a_register_carrying_route_is_violated() {
        let f = routeless(&[register(1_000, "z9hG4bK-1", "<sip:a@1>", "Route: <sip:p@h;lr>\r\n")]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].rule, RuleId::RegisterNoRouteSet);
        let Decision::Violated(Evidence::RegisterCarriesRoute { routes, .. }) = &f[0].decision
        else {
            panic!("register-route evidence: {:?}", f[0].decision)
        };
        assert_eq!(routes.as_slice(), ["<sip:p@h;lr>"]);
    }

    /// A vantage with no header block cannot say what the REGISTER carried:
    /// the occasion stands, UNDECIDED.
    #[test]
    fn a_register_without_header_bytes_is_undecidable() {
        let mut m = register(1_000, "z9hG4bK-1", "<sip:a@1>", "");
        m.head = None;
        let f = routeless(&[m.clone()]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(!f[0].decided(), "{:?}", f[0].decision);
        let s = serial(&[m]);
        assert_eq!(s.len(), 1, "{s:?}");
        assert!(!s[0].decided(), "{:?}", s[0].decision);
    }

    /// A retransmitted REGISTER is the same binding attempt again.
    #[test]
    fn a_retransmitted_register_is_not_a_second_occasion() {
        let mut again = register(2_000, "z9hG4bK-1", "<sip:a@1>", "Route: <sip:p@h;lr>\r\n");
        again.repeat = true;
        let msgs = [register(1_000, "z9hG4bK-1", "<sip:a@1>", "Route: <sip:p@h;lr>\r\n"), again];
        assert_eq!(routeless(&msgs).len(), 1, "{:?}", routeless(&msgs));
    }

    /// The obligation met: the first binding was answered before the Contact
    /// changed.
    #[test]
    fn a_contact_change_after_the_answer_is_compliant() {
        let f = serial(&[
            register(1_000, "z9hG4bK-1", "<sip:a@1>", ""),
            answered(2_000, "z9hG4bK-1", 200),
            register(3_000, "z9hG4bK-2", "<sip:a@2>", ""),
        ]);
        assert_eq!(f.len(), 2, "one occasion per REGISTER: {f:?}");
        assert!(f.iter().all(|x| matches!(x.decision, Decision::Compliant)), "{f:?}");
    }

    /// The violation: two bindings for one AOR in flight at once, and the
    /// registrar decides which survives by arrival order.
    #[test]
    fn a_contact_change_while_one_is_outstanding_is_violated() {
        let f = serial(&[
            register(1_000, "z9hG4bK-1", "<sip:a@1>", ""),
            register(2_000, "z9hG4bK-2", "<sip:a@2>", ""),
        ]);
        assert_eq!(f.len(), 2, "{f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
        assert_eq!(f[1].rule, RuleId::SerialRegister);
        assert_eq!(f[1].emitter, UAC, "the sender is charged");
        let Decision::Violated(Evidence::ConcurrentRegister {
            aor,
            contact,
            pending_contact,
            pending_branch,
            branch,
            ..
        }) = &f[1].decision
        else {
            panic!("serial-register evidence: {:?}", f[1].decision)
        };
        assert_eq!(aor, "sip:alice@h");
        assert_eq!((contact.as_str(), pending_contact.as_str()), ("<sip:a@2>", "<sip:a@1>"));
        assert_eq!((branch.as_str(), pending_branch.as_str()), ("z9hG4bK-2", "z9hG4bK-1"));
    }

    /// Re-sending the SAME Contact is a retry of one binding attempt, not a
    /// racing second one.
    #[test]
    fn resending_the_same_contact_is_compliant() {
        let f = serial(&[
            register(1_000, "z9hG4bK-1", "<sip:a@1>", ""),
            register(2_000, "z9hG4bK-2", "<sip:a@1>", ""),
        ]);
        assert_eq!(f.len(), 2, "{f:?}");
        assert!(f.iter().all(|x| matches!(x.decision, Decision::Compliant)), "{f:?}");
    }

    /// An answer that arrives AFTER the second REGISTER left did not license
    /// it: the discharge is read at its own place in the view.
    #[test]
    fn an_answer_after_the_second_send_does_not_excuse_it() {
        let f = serial(&[
            register(1_000, "z9hG4bK-1", "<sip:a@1>", ""),
            register(2_000, "z9hG4bK-2", "<sip:a@2>", ""),
            answered(3_000, "z9hG4bK-1", 200),
        ]);
        assert_eq!(f.len(), 2, "{f:?}");
        assert!(f[1].violated(), "{:?}", f[1].decision);
    }
}
