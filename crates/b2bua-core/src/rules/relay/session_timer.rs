//! Whether a request this stack mints toward a leg carries the session-timer
//! negotiation of the request it copies (RFC 4028). The stack runs no timer
//! of its own: it is transparent to the endpoints' timer, and so only a leg
//! whose answers reach the source's sender may carry that negotiation.

use sip_message::generators;
use sip_message::SipHeader as MsgHeader;

use super::originate::apply_withheld_option_tags;

/// The option tag naming the session timer (RFC 4028 §3).
pub(crate) const TIMER: &str = "timer";

/// Settle the session timer on `minted`, the headers of a request copying
/// `source` toward a leg.
///
/// `takes_part` is false where the leg's answer never reaches the source's
/// sender (nobody would refresh it) or the call withholds `timer` from the
/// leg: the minted request then neither offers nor negotiates the timer.
/// Otherwise `leg_offers` states whether the leg's own `Supported` names
/// `timer`; a leg that lost the tag the source offered loses the interval,
/// floor and demand with it (RFC 4028 §7.1: a UAC using the timer lists it).
pub(crate) fn settle_session_timer(
    minted: &mut Vec<MsgHeader>,
    source: &[MsgHeader],
    takes_part: bool,
    leg_offers: bool,
) {
    if !takes_part {
        apply_withheld_option_tags(minted, &[TIMER.to_string()]);
        generators::withdraw_session_timer(minted);
    } else if !leg_offers && generators::offers_session_timer(source) {
        generators::withdraw_session_timer(minted);
    }
}
