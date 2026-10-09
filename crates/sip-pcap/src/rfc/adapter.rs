//! The capture adapter: one leg of an emitted flows document
//! projected into the `rfc-rules` wire model, and the merged rules' findings
//! folded back into the census [`Scan`].
//!
//! Observation policy lives here, not in the rules: the capture is an OPEN
//! observation (`closed: false` — the recording span, not the harness, bounds
//! the evidence), and every rule of the [`RfcRule::WIRE`] contract — all of
//! them — is run.

use rfc_rules::{Decision, Kind, Msg, Observation, WireView};

use crate::doc::{MsgJson, Payload, Summary};

use super::{RfcRule, Scan, Site};

pub(super) fn detect(at: &Site<'_>, out: &mut Scan, candidates: &[RfcRule]) {
    let msgs: Vec<Msg> = at.leg.msgs.iter().map(|m| msg_of(m, &at.leg.call_id)).collect();
    let obs = Observation {
        last_us: at.span.last_us,
        endpoint_last_us: at.span.endpoint_last_us.clone(),
        closed: false,
    };
    let view = WireView { msgs: &msgs, obs: &obs };
    // Every rule with a body: `candidates` names nothing beyond them.
    let _ = candidates;
    for rule in rfc_rules::all_rules() {
        for f in rule.eval(&view) {
            out.count(f.rule, f.decided());
            if let Decision::Violated(evidence) = &f.decision {
                out.hits.push(at.hit(&f, evidence.clone()));
            }
        }
    }
}

/// One captured message reduced to the wire-model facts, off the summary the
/// emitter already extracted — never re-parsed, so the adapter judges exactly
/// what the document states. A leg IS one call, so its Call-ID is every
/// message's.
fn msg_of(m: &MsgJson, call_id: &str) -> Msg {
    let (kind, cseq, from, to) = match &m.summary {
        Summary::Request { method, cseq, from, to, .. } => {
            (Kind::Request { method: method.clone() }, cseq, from, to)
        }
        Summary::Response { status, cseq, from, to, .. } => {
            (Kind::Response { status: *status }, cseq, from, to)
        }
    };
    Msg {
        at_us: m.ts_us,
        src: m.src.clone(),
        dst: m.dst.clone(),
        hop: m.hop,
        repeat: m.repeat_of.is_some(),
        kind,
        call_id: call_id.to_string(),
        cseq: cseq.seq,
        cseq_method: cseq.method.clone(),
        via_branch: m.via.first().and_then(|v| v.branch.clone()),
        from_tag: from.tag.clone(),
        to_tag: to.tag.clone(),
        head: head_bytes(&m.payload),
        body: m.payload.body(),
    }
}

/// The message's header block as bytes, or `None` where even its head is not
/// text — a datagram nothing can read a header out of.
fn head_bytes(payload: &Payload) -> Option<Vec<u8>> {
    match payload {
        Payload::Text { raw } => Some(raw.as_bytes().to_vec()),
        Payload::HeadBody { head, .. } => Some(head.as_bytes().to_vec()),
        Payload::Opaque { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::callfacts::{mark_repeats, REPEAT_ENVELOPE_US};
    use crate::doc::{CSeqJson, LegJson, Party, ViaJson};

    fn answer(ts_us: u64) -> MsgJson {
        let mut m = MsgJson::new(
            ts_us,
            "a".into(),
            "b".into(),
            0,
            Payload::Text { raw: String::new() },
            Summary::Response {
                status: 200,
                reason: "OK".into(),
                cseq: CSeqJson { seq: 1, method: "INVITE".into() },
                from: Party { uri: "sip:a@h".into(), tag: None },
                to: Party { uri: "sip:b@h".into(), tag: None },
            },
        );
        m.via = vec![ViaJson {
            sent_by: "a".into(),
            transport: "UDP".into(),
            branch: Some("z9hG4bK1".into()),
            received: None,
        }];
        m
    }

    /// The rules skip a repeat, so the envelope decides what they SEE: a final
    /// re-sent past 64·T1 reaches them as a fresh event.
    #[test]
    fn a_re_emission_reaches_the_wire_model_unmarked() {
        let mut leg = LegJson {
            call_id: "c1".into(),
            hops: Vec::new(),
            invite: None,
            final_status: None,
            saw_180: false,
            terminated_by: None,
            tokens: Vec::new(),
            msgs: vec![answer(0), answer(1_000_000), answer(REPEAT_ENVELOPE_US + 1)],
        };
        mark_repeats(&mut leg);
        let seen: Vec<bool> = leg.msgs.iter().map(|m| msg_of(m, &leg.call_id).repeat).collect();
        assert_eq!(seen, vec![false, true, false]);
    }
}
