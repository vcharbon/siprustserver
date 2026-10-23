//! What a PRACK this stack receives draws (RFC 3262 §3): the one reading the
//! live call answers a PRACK by and the released call's late-PRACK book is
//! built from. On every face this stack is the UAS a PRACK is addressed to —
//! the `RSeq` there is its own — so it owes the 481 of a PRACK naming nothing
//! unacknowledged, and the 200 of one whose responder needs no relayed copy.

use crate::model::{CSeq, Call, Ladder, Obligation, Provisional, RSeq};

/// The three tokens a PRACK's `RAck` carries (RFC 3262 §7.2), as read off the
/// request: the response-num, the CSeq-num, and whether the method token is
/// `INVITE` — the only method a reliable provisional ever answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RAckTokens {
    pub rseq: RSeq,
    pub cseq: CSeq,
    pub names_invite: bool,
}

/// What a PRACK draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrackDisposition {
    /// It acknowledges a shown provisional whose responder still awaits a
    /// PRACK on a dialog that carries one: relayed, its `RAck` translated.
    Relay,
    /// It acknowledges a shown provisional whose responder needs no relayed
    /// PRACK — PRACKed by this stack already, or its dialog gone: 200 here.
    Answer200,
    /// It names no provisional shown in its dialog, or one its party already
    /// acknowledged: 481 here, never relayed (§3).
    Refuse481,
}

/// The shown provisional a PRACK in this stack's `tag` dialog naming `rack`
/// matches on all three §7.2 tokens.
pub fn named_by<'a>(call: &'a Call, tag: &str, rack: RAckTokens) -> Option<&'a Provisional> {
    if !rack.names_invite {
        return None;
    }
    call.provisionals.iter().find(|p| {
        p.shown
            .as_ref()
            .is_some_and(|s| s.tag == tag && s.rseq == rack.rseq && s.invite_cseq == rack.cseq)
    })
}

/// What a PRACK in this stack's `tag` dialog naming `rack` draws. `discharged`
/// is the ladder the engine retired for this very PRACK before the rules ran:
/// the PRACK that acknowledged a provisional is answered for it, a later one
/// naming it is refused.
pub fn prack_disposition(
    call: &Call,
    tag: &str,
    rack: RAckTokens,
    discharged: Option<&Obligation>,
) -> PrackDisposition {
    let Some(p) = named_by(call, tag, rack) else { return PrackDisposition::Refuse481 };
    let this_one = discharged.is_some_and(
        |o| matches!(o, Obligation::AwaitPrack { tag: t, rseq } if t == tag && *rseq == rack.rseq),
    );
    if p.acknowledged_by_shown() && !this_one {
        return PrackDisposition::Refuse481;
    }
    owed_by_responder(call, p)
}

/// What a PRACK acknowledging the shown provisional `p` draws, by its
/// responder's face alone.
fn owed_by_responder(call: &Call, p: &Provisional) -> PrackDisposition {
    if p.responder.is_owed() && responder_dialog_carries_prack(call, p) {
        PrackDisposition::Relay
    } else {
        PrackDisposition::Answer200
    }
}

/// Whether the responder's dialog of `p` still carries a new request: live,
/// and not one whose INVITE this side is cancelling or whose BYE is in flight.
pub fn responder_dialog_carries_prack(call: &Call, p: &Provisional) -> bool {
    crate::helpers::find_leg(call, &p.id.leg)
        .and_then(|l| crate::helpers::dialog_with_peer(l, &p.id.responder_tag))
        .is_some_and(|d| d.carries_requests())
}

/// One dialog's shown provisionals a PRACK still draws 200 for once the call
/// is released: the dialog's `Call-ID`, this stack's tag in it, and each
/// provisional's `(RSeq, CSeq-num)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnswerableShown {
    pub call_id: String,
    pub tag: String,
    pub racks: Vec<(RSeq, CSeq)>,
}

/// The shown provisionals of `call` a first PRACK draws 200 for, grouped per
/// shown dialog: unacknowledged by the party shown them, their responder
/// owed no relayed PRACK. A tag no dialog carries is a caller-facing fork
/// tag, the a-leg's ([`crate::helpers::leg_shown`]).
pub fn answerable_shown(call: &Call) -> Vec<AnswerableShown> {
    let mut out: Vec<AnswerableShown> = Vec::new();
    for p in &call.provisionals {
        let Some(s) = p.shown.as_ref() else { continue };
        if s.ladder == Ladder::Acknowledged
            || owed_by_responder(call, p) != PrackDisposition::Answer200
        {
            continue;
        }
        let call_id = crate::helpers::leg_shown(call, &s.tag)
            .and_then(|id| crate::helpers::find_leg(call, id))
            .unwrap_or(&call.a_leg)
            .call_id
            .clone();
        match out.iter_mut().find(|u| u.call_id == call_id && u.tag == s.tag) {
            Some(u) => u.racks.push((s.rseq, s.invite_cseq)),
            None => out.push(AnswerableShown {
                call_id,
                tag: s.tag.clone(),
                racks: vec![(s.rseq, s.invite_cseq)],
            }),
        }
    }
    out
}
