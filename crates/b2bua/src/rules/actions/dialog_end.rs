//! A leg's dialogs moving to the state its teardown records (RFC 3261 §15):
//! this stack's BYE leaves a dialog `Ending`, the peer's BYE or the end of
//! this stack's BYE transaction leaves it `Ended`.

use call::{ByeDisposition, Call, DialogEnd, DialogState, Leg};
use sip_message::Method as SipMethod;

use b2bua_sdk::model::RuleContext;

/// Move the dialogs of `leg_id` to the state `bye` records. The peer's BYE
/// ends the dialog it names when the turn's event is that BYE, else the
/// leg's confirmed dialog; the end of this stack's BYE ends the dialog that
/// BYE left `Ending`, with the final the turn's event carries.
pub(super) fn end_dialogs(call: &mut Call, ctx: &RuleContext, leg_id: &str, bye: ByeDisposition) {
    let on_leg = ctx.source_leg_id == leg_id;
    // The dialog the BYE names, as the dialog gate resolved it.
    let bye_dialog = ctx
        .request()
        .filter(|r| on_leg && *r.method() == SipMethod::Bye)
        .and_then(|_| ctx.dialog())
        .filter(|d| d.retired().is_none())
        .map(|d| d.id());
    let bye_final = ctx
        .response()
        .filter(|r| on_leg && r.cseq().method() == SipMethod::Bye && r.status() >= 200)
        .map(|r| r.status());
    *call = call::helpers::update_leg(call.clone(), leg_id, |leg| match bye {
        ByeDisposition::ByeSent => {
            if let Some(d) = leg.dialogs.confirmed_mut() {
                d.state = DialogState::Ending;
            }
        }
        ByeDisposition::ByeReceived => end_one(leg, bye_dialog.clone(), DialogEnd::PeerBye),
        ByeDisposition::ByeConfirmed => {
            end_one(leg, None, DialogEnd::OurBye { final_status: bye_final });
        }
        ByeDisposition::ByeTimeout => end_one(leg, None, DialogEnd::OurBye { final_status: None }),
        ByeDisposition::Cancelled | ByeDisposition::Rejected | ByeDisposition::None => {}
    });
}

/// Mark the leg's dialog `id` names — else its confirmed one — `Ended(end)`.
fn end_one(leg: &mut Leg, id: Option<call::DialogId>, end: DialogEnd) {
    let d = match id {
        Some(id) => leg.dialogs.by_tags_mut(&id.our_tag, &id.peer_tag),
        None => leg.dialogs.confirmed_mut(),
    };
    if let Some(d) = d {
        d.state = DialogState::Ended(end);
    }
}
