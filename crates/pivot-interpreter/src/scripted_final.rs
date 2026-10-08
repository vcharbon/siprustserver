//! Which INVITE a final a send step scripts answers, and whether that INVITE
//! already has its final (RFC 3261 §17.2.1: one per server transaction).
//!
//! A scripted final answers the INVITE its leg took at the last `expect` of an
//! INVITE ahead of it on that leg; the ladder names that INVITE through the
//! step its arrival was attributed to. Until it arrives the final answers
//! nothing yet, so it is neither moot nor an answer to any CANCEL.

use pivot_schema::bundle::{Dir, RecordedMessage};
use sip_message::parser::custom::CustomParser;
use sip_message::{Method, SipMessage, SipParser};

use crate::plan::{CompiledStep, Discriminator};

/// Whether `step` sends a final response to an INVITE.
pub fn is_final_to_invite(step: &CompiledStep) -> bool {
    step.is_send()
        && matches!(&step.discriminator,
            Discriminator::Response { status, cseq_method: Some(method) }
                if *status >= 200 && Method::from_wire(method) == Method::Invite)
}

/// Whether `step` takes an INVITE.
fn takes_invite(step: &CompiledStep) -> bool {
    step.is_expect()
        && matches!(&step.discriminator, Discriminator::Request { method }
            if Method::from_wire(method) == Method::Invite)
}

/// The CSeq of the INVITE the final `steps[at]` answers, read off `ladder` (its
/// leg's recording). `None` while that INVITE has not arrived, or where no
/// step ahead of it takes one.
pub fn answered_invite(
    steps: &[&CompiledStep],
    at: usize,
    ladder: &[RecordedMessage],
) -> Option<u32> {
    let leg = &steps.get(at)?.leg;
    let took = steps[..at].iter().rev().find(|s| &s.leg == leg && takes_invite(s))?;
    ladder
        .iter()
        .filter(|m| m.dir == Dir::In && m.step.as_deref() == Some(took.id.as_str()))
        .find_map(|m| match parse(m.wire())? {
            SipMessage::Request(r) => Some(r.cseq().seq()),
            SipMessage::Response(_) => None,
        })
}

/// Whether `ladder`'s leg already sent a final to its INVITE CSeq `cseq`.
pub fn invite_answered(ladder: &[RecordedMessage], cseq: u32) -> bool {
    ladder.iter().filter(|m| m.dir == Dir::Out).any(|m| {
        matches!(parse(m.wire()), Some(SipMessage::Response(r))
            if r.status() >= 200
                && r.cseq().seq() == cseq
                && *r.cseq().method() == Method::Invite)
    })
}

/// The non-2xx final `leg`'s flow still owes its INVITE CSeq `cseq`: the first
/// pending (`!complete`) scripted final answering that INVITE, where it
/// rejects. `None` where none is pending or that final is a 2xx — a CANCELled
/// INVITE is never answered 2xx (§9.2).
pub fn pending_reject<'s>(
    steps: &[&'s CompiledStep],
    leg: &str,
    cseq: u32,
    ladder: &[RecordedMessage],
    complete: impl Fn(&str) -> bool,
) -> Option<&'s CompiledStep> {
    let (_, step) = steps.iter().copied().enumerate().find(|(at, s)| {
        s.leg == leg
            && is_final_to_invite(s)
            && !complete(&s.id)
            && answered_invite(steps, *at, ladder) == Some(cseq)
    })?;
    matches!(&step.discriminator, Discriminator::Response { status, .. } if *status >= 300)
        .then_some(step)
}

fn parse(wire: &[u8]) -> Option<SipMessage> {
    CustomParser::new().parse(wire).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::Plan;
    use crate::recording::Recording;

    fn plan(flow: &str) -> Plan {
        let text = format!(
            r#"{{
              "pivot_version": 3,
              "case": {{ "id": "t", "title": "t", "family": "transparent", "variant": "repro",
                        "origin": "authored", "lanes": {{ "upstream-fake": "ok" }} }},
              "identities": [ {{ "name": "caller", "kind": "external-caller", "forms": ["private"] }} ],
              "calls": [ {{ "id": "c1", "caller_leg": "A", "attempts": [
                 {{ "branch": 0, "position": 0, "leg": "B", "callee": {{ "identity": "caller" }} }} ] }} ],
              "endpoints": [ {{ "id": "ep0", "observed": "127.0.0.1:5060", "side": "peer", "binding": "dedicated" }} ],
              "actors": [ {{ "id": "uac1", "kind": "uac", "endpoint": "ep0" }},
                          {{ "id": "uas1", "kind": "uas", "endpoint": "ep0" }} ],
              "legs": [ {{ "id": "A", "actor": "uac1", "dir": "out" }},
                        {{ "id": "B", "actor": "uas1", "dir": "in" }} ],
              "flow": {flow},
              "postconditions": {{ "cdr": {{ "absent": "unit test" }} }},
              "timing": {{ "expect_budget_ms": 1000, "settle_budget_ms": 1000 }}
            }}"#
        );
        let document = pivot_schema::PivotV3::from_json(&text).expect("the fixture parses");
        Plan::compile(document).expect("the fixture compiles")
    }

    const D: &str = r#"{"ms":0,"from":"trigger","compressible":true,"timer_linked":false}"#;

    fn take(id: &str) -> String {
        format!(
            r#"{{"id":"{id}","leg":"B","op":"expect","check":"record","msg":{{"method":"INVITE"}},"delay":{D}}}"#
        )
    }

    fn answer(id: &str, status: u16) -> String {
        format!(
            r#"{{"id":"{id}","leg":"B","op":"send","msg":{{"status":{status},"cseq-method":"INVITE"}},"delay":{D}}}"#
        )
    }

    fn invite(cseq: u32, to_tag: Option<&str>) -> String {
        let tag = to_tag.map(|t| format!(";tag={t}")).unwrap_or_default();
        format!(
            "INVITE sip:bob@127.0.0.1:5080 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-{cseq}\r\n\
             From: <sip:alice@example.test>;tag=a1\r\n\
             To: <sip:bob@example.test>{tag}\r\n\
             Call-ID: call-b\r\n\
             CSeq: {cseq} INVITE\r\n\
             Content-Length: 0\r\n\r\n"
        )
    }

    fn final_to(status: u16, cseq: u32) -> String {
        format!(
            "SIP/2.0 {status} Whatever\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-{cseq}\r\n\
             From: <sip:alice@example.test>;tag=a1\r\n\
             To: <sip:bob@example.test>;tag=b1\r\n\
             Call-ID: call-b\r\n\
             CSeq: {cseq} INVITE\r\n\
             Content-Length: 0\r\n\r\n"
        )
    }

    /// Leg B's ladder: `(dir, raw, step)` in wire order.
    fn ladder(entries: &[(Dir, String, Option<&str>)]) -> Vec<RecordedMessage> {
        let recording = Recording::new();
        recording.declare("B");
        for (at, (dir, raw, step)) in entries.iter().enumerate() {
            recording.push("B", *dir, at as u64, raw.clone(), *step, None);
        }
        recording.legs().remove("B").expect("the leg was recorded")
    }

    /// Two INVITEs on one leg: the reject the flow scripts behind the SECOND
    /// answers that one, and is no answer owed the first.
    #[test]
    fn a_reject_answers_the_invite_its_own_expect_took() {
        let flow =
            format!("[{},{},{},{}]", take("t1"), answer("r1", 200), take("t2"), answer("r2", 486));
        let plan = plan(&flow);
        let steps = plan.steps();
        let rec = ladder(&[
            (Dir::In, invite(1, None), Some("t1")),
            (Dir::Out, final_to(200, 1), Some("r1")),
            (Dir::In, invite(2, Some("b1")), Some("t2")),
        ]);
        let done = |id: &str| id != "r2";
        assert_eq!(answered_invite(&steps, 3, &rec), Some(2));
        assert!(pending_reject(&steps, "B", 1, &rec, done).is_none(), "r2 answers CSeq 2, not 1");
        assert_eq!(pending_reject(&steps, "B", 2, &rec, done).map(|s| s.id.as_str()), Some("r2"));
    }

    /// A scripted final whose INVITE has not arrived answers nothing yet: it is
    /// no answer to the INVITE before it, already answered — so not moot.
    #[test]
    fn a_final_whose_invite_has_not_arrived_answers_nothing_yet() {
        let flow =
            format!("[{},{},{},{}]", take("t1"), answer("r1", 200), take("t2"), answer("r2", 488));
        let plan = plan(&flow);
        let steps = plan.steps();
        let rec = ladder(&[
            (Dir::In, invite(1, None), Some("t1")),
            (Dir::Out, final_to(200, 1), Some("r1")),
        ]);
        assert_eq!(answered_invite(&steps, 3, &rec), None, "t2's INVITE is still to come");
        let moot = answered_invite(&steps, 3, &rec).is_some_and(|c| invite_answered(&rec, c));
        assert!(!moot, "r2 answers nothing yet: it stays to be composed, loudly");
        assert!(invite_answered(&rec, 1));
        assert!(!invite_answered(&rec, 2));
    }

    /// §9.2: a scripted 2xx is no answer to a CANCELled INVITE.
    #[test]
    fn a_pending_2xx_is_no_reject() {
        let flow = format!("[{},{}]", take("t1"), answer("r1", 200));
        let plan = plan(&flow);
        let steps = plan.steps();
        let rec = ladder(&[(Dir::In, invite(1, None), Some("t1"))]);
        assert!(pending_reject(&steps, "B", 1, &rec, |_| false).is_none());
    }
}
