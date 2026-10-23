//! The B2BUA's own per-leg identity (Via + Contact) on outbound messages,
//! binding [`B2buaConfig`]'s addresses to [`crate::stack_identity`]'s wire
//! shapes (call-ref/leg/incarnation markers, `;em=1`/`;emerg=1` emergency
//! stamps).

use call::Call;
use sip_message::header::{self, Via};

use crate::config::B2buaConfig;
use crate::stack_identity::{build_call_contact, build_call_via, StackIdentityOpts};

/// What of a call the stack stamps on every Via and Contact it emits for it:
/// its `call_ref`, its incarnation's mark, and its emergency state, whose
/// `;em=1` / `;emerg=1` markers keep an admitted emergency call identifiable
/// on the wire in every subsequent in-dialog packet.
#[derive(Clone, Copy, Debug)]
pub struct CallMarks<'a> {
    pub call_ref: &'a str,
    /// [`Call::incarnation_mark`].
    pub incarnation_mark: &'a str,
    /// `call.emergency == Some(true)`.
    pub is_emergency: bool,
}

impl<'a> CallMarks<'a> {
    /// The marks of `call`.
    pub fn of(call: &'a Call) -> Self {
        Self {
            call_ref: &call.call_ref,
            incarnation_mark: call.incarnation_mark(),
            is_emergency: call.emergency == Some(true),
        }
    }

    fn opts<'b>(&self, config: &'b B2buaConfig, leg_id: &'b str) -> StackIdentityOpts<'b>
    where
        'a: 'b,
    {
        StackIdentityOpts {
            local_ip: &config.sip_local_ip,
            local_port: config.sip_local_port,
            call_ref: self.call_ref,
            leg: leg_id,
            incarnation_mark: self.incarnation_mark,
            is_emergency: self.is_emergency,
        }
    }
}

/// The B2BUA's Via for a leg's outbound message.
pub fn leg_via(config: &B2buaConfig, marks: CallMarks, leg_id: &str, branch: String) -> Via {
    build_call_via(&marks.opts(config, leg_id), branch)
}

/// The B2BUA's Contact for a leg's outbound message.
pub fn leg_contact(config: &B2buaConfig, marks: CallMarks, leg_id: &str) -> header::Contact {
    build_call_contact(&marks.opts(config, leg_id))
}
