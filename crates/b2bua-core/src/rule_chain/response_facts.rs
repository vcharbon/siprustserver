//! What a response to an INVITE records before the rules read it: that the
//! callee rang, with the ring deadline its first 18x arms, and the end a
//! non-2xx final makes of the transaction and of the early dialogs it created.

use call::Call;
use sip_message::{Method, SipResponse};

use crate::effects::HandlerEffects;
use crate::rules::actions::{Owed, Settle};
use crate::rules::ActionExecutor;

/// Record what `resp` from `leg` says before the rules run. `screened`: the
/// engine dropped it as a reliable provisional's repeat or one out of `RSeq`
/// order, which records nothing. A 180–189 marks the callee rang; the first
/// arms the leg's ring deadline (`ring_deadline`). A non-2xx
/// INVITE final ends the transaction it answers (RFC 3261 §17.1.1.3,
/// hop-ACKed below the TU), so the record stops naming that round as in
/// flight; a final to the INVITE that dialled the leg ends every early dialog
/// it created (§12.1), the PRACKs its responders are owed leaving first (RFC
/// 3262 §4).
pub(super) fn record(
    exec: &mut ActionExecutor,
    call: &mut Call,
    fx: &mut HandlerEffects,
    leg: &str,
    resp: &SipResponse,
    screened: bool,
) {
    if resp.cseq().method() != Method::Invite || screened {
        return;
    }
    if (180..=189).contains(&resp.status()) {
        let cseq = i64::from(resp.cseq().seq());
        crate::ring_deadline::arm_at_first_18x(exec.config, call, fx, leg, cseq, exec.now_ms);
        call::helpers::mark_rang(call, leg);
    }
    if resp.status() < 300 {
        return;
    }
    let cseq = i64::from(resp.cseq().seq());
    exec.settle_owed_pracks(
        call,
        fx,
        Owed::Invite { leg, cseq },
        Settle::AtFinal { confirmed: None },
    );
    if let Some(branch) = resp.top_via().branch() {
        call::helpers::close_rejected_invite_round(call, leg, branch);
    }
    call::helpers::record_dialling_invite_rejected(call, leg, cseq, resp.status());
}
