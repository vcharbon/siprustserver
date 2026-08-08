//! The B2BUA's own per-leg identity (Via + Contact) on outbound messages,
//! binding [`B2buaConfig`]'s addresses to [`crate::stack_identity`]'s wire
//! shapes (call-ref/leg markers, `;em=1`/`;emerg=1` emergency stamps).

use sip_message::header::{self, Via};

use crate::config::B2buaConfig;
use crate::stack_identity::{build_call_contact, build_call_via, StackIdentityOpts};

/// The B2BUA's Via for a leg's outbound message. `is_emergency` is the call's
/// emergency state (`call.emergency == Some(true)`); when set it stamps the
/// `;em=1` marker every subsequent in-dialog packet of the call then carries,
/// so an admitted emergency call stays identifiable on the wire.
pub fn leg_via(
    config: &B2buaConfig,
    call_ref: &str,
    leg_id: &str,
    is_emergency: bool,
    branch: String,
) -> Via {
    build_call_via(
        &StackIdentityOpts {
            local_ip: &config.sip_local_ip,
            local_port: config.sip_local_port,
            call_ref,
            leg: leg_id,
            is_emergency,
        },
        branch,
    )
}

/// The B2BUA's Contact for a leg's outbound message. `is_emergency` (the call's
/// `call.emergency == Some(true)`) stamps the `;emerg=1` Contact marker — see
/// [`leg_via`].
pub fn leg_contact(
    config: &B2buaConfig,
    call_ref: &str,
    leg_id: &str,
    is_emergency: bool,
) -> header::Contact {
    build_call_contact(&StackIdentityOpts {
        local_ip: &config.sip_local_ip,
        local_port: config.sip_local_port,
        call_ref,
        leg: leg_id,
        is_emergency,
    })
}
