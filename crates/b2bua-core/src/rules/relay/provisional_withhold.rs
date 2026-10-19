//! The headers a call's decision leaves behind on the provisional responses
//! the stack relays toward the originator
//! (`features.withhold_on_relayed_provisionals`).

use call::Call;
use sip_message::header::HeaderName;
use sip_message::SipHeader;

/// Remove from `headers`, the relayed lines of a `status` response to the
/// originator's INVITE, every line `call` withholds from a provisional
/// response. A final response and a call withholding nothing keep every line.
pub fn withhold_from_relayed_provisional(call: &Call, status: u16, headers: &mut Vec<SipHeader>) {
    if !(101..200).contains(&status) {
        return;
    }
    let Some(names) =
        call.features.as_ref().and_then(|f| f.withhold_on_relayed_provisionals.as_deref())
    else {
        return;
    };
    let withheld: Vec<HeaderName> = names.iter().map(|n| HeaderName::from(n.as_str())).collect();
    headers.retain(|h| !withheld.iter().any(|w| w.matches(&h.name)));
}
