//! The B2BUA's own per-leg identity (Via + Contact) on outbound messages,
//! binding [`B2buaConfig`]'s addresses to [`crate::stack_identity`]'s wire
//! shapes (call-ref/leg/incarnation markers, `;em=1`/`;emerg=1` emergency
//! stamps, the call's other owner and the signed via-extension on the Via).

use call::Call;
use sip_message::header::{self, Via};

use crate::config::B2buaConfig;
pub use crate::stack_identity::RequestFacts;
use crate::stack_identity::{build_call_contact, build_call_via, StackIdentityOpts};

/// What of a call the stack stamps on every Via and Contact it emits for it:
/// its `call_ref`, its incarnation's mark, its emergency state, whose
/// `;em=1` / `;emerg=1` markers keep an admitted emergency call identifiable
/// on the wire in every subsequent in-dialog packet, and its two owners.
#[derive(Clone, Copy, Debug)]
pub struct CallMarks<'a> {
    pub call_ref: &'a str,
    /// [`Call::incarnation_mark`].
    pub incarnation_mark: &'a str,
    /// `call.emergency == Some(true)`.
    pub is_emergency: bool,
    /// The call's primary and backup, `None` for a call placed without them.
    pub owners: Option<(&'a str, &'a str)>,
}

impl<'a> CallMarks<'a> {
    /// The marks of `call`.
    pub fn of(call: &'a Call) -> Self {
        Self {
            call_ref: &call.call_ref,
            incarnation_mark: call.incarnation_mark(),
            is_emergency: call.emergency == Some(true),
            owners: call.topology.as_ref().map(|t| (t.pri.as_str(), t.bak.as_str())),
        }
    }

    /// The owner of the call other than `sender`: the backup for the primary,
    /// the primary for a copy on the backup. `None` for a call with one owner.
    fn other_owner(&self, sender: &str) -> Option<&'a str> {
        let (pri, bak) = self.owners?;
        let other = if sender == bak { pri } else { bak };
        (!other.is_empty() && other != sender).then_some(other)
    }

    fn opts<'b>(
        &self,
        config: &'b B2buaConfig,
        leg_id: &'b str,
        request: Option<RequestFacts<'b>>,
    ) -> StackIdentityOpts<'b>
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
            other_owner: self.other_owner(&config.self_ordinal),
            request: request.zip(config.cluster_key.as_ref()),
        }
    }
}

/// The B2BUA's Via for a leg's outbound message; a request that states its
/// facts carries them, signed, in the via-extension (ADR-0055 §12).
pub fn leg_via(
    config: &B2buaConfig,
    marks: CallMarks,
    leg_id: &str,
    request: Option<RequestFacts>,
    branch: String,
) -> Via {
    build_call_via(&marks.opts(config, leg_id, request), branch)
}

/// The B2BUA's Contact for a leg's outbound message.
pub fn leg_contact(config: &B2buaConfig, marks: CallMarks, leg_id: &str) -> header::Contact {
    build_call_contact(&marks.opts(config, leg_id, None))
}
