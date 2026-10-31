//! The B2BUA's own per-leg identity (Via + Contact) on outbound messages,
//! binding [`B2buaConfig`]'s addresses to [`crate::stack_identity`]'s wire
//! shapes (call-ref/leg/incarnation markers, `;em=1`/`;emerg=1` emergency
//! stamps, the call's owners and the signed via-extension on the Via).
//! The Via names the sending node; a leg's Contact is allocated once, when
//! the leg is created, and read back from the record ever after.

use call::Call;
use sip_message::header::{self, Uri, Via};
use sip_message::SipStr;

use crate::config::B2buaConfig;
use crate::owners::Owners;
pub use crate::stack_identity::RequestFacts;
use crate::stack_identity::{build_call_contact, build_call_via, ContactFacts, StackIdentityOpts};

/// What of a call the stack stamps on every Via and Contact it emits for it:
/// its `call_ref`, its incarnation's mark, its emergency state, whose
/// `;em=1` / `;emerg=1` markers keep an admitted emergency call identifiable
/// on the wire in every subsequent in-dialog packet, and its owners.
#[derive(Clone, Copy, Debug)]
pub struct CallMarks<'a> {
    pub call_ref: &'a str,
    /// [`Call::incarnation_mark`].
    pub incarnation_mark: &'a str,
    /// `call.emergency == Some(true)`.
    pub is_emergency: bool,
    /// The call's owners: this node, named on the records of the requests it
    /// sends, and the owners stated on the Via.
    pub owners: &'a Owners,
}

impl<'a> CallMarks<'a> {
    /// The marks of `call`, owned by `owners`.
    pub fn of(call: &'a Call, owners: &'a Owners) -> Self {
        Self {
            call_ref: &call.call_ref,
            incarnation_mark: call.incarnation_mark(),
            is_emergency: call.emergency == Some(true),
            owners,
        }
    }

    /// The Via options of a request on `leg_id`. One that `dials` the leg
    /// states the call's primary and its backup; any other, the other owner.
    fn opts<'b>(
        &self,
        config: &'b B2buaConfig,
        leg_id: &'b str,
        request: Option<RequestFacts<'b>>,
        dials: bool,
    ) -> StackIdentityOpts<'b>
    where
        'a: 'b,
    {
        let owners = self.owners;
        let primary = owners.primary.as_deref().filter(|_| dials);
        let backup =
            if primary.is_some() { owners.backup.as_deref() } else { owners.other.as_deref() };
        StackIdentityOpts {
            local_ip: &config.sip_local_ip,
            local_port: config.sip_local_port,
            call_ref: self.call_ref,
            leg: leg_id,
            incarnation_mark: self.incarnation_mark,
            is_emergency: self.is_emergency,
            backup,
            primary,
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
    build_call_via(&marks.opts(config, leg_id, request, false), branch)
}

/// [`leg_via`] for the request that dials leg `leg_id`, its dialog-creating
/// INVITE: it states the call's primary and backup whichever owner sends it,
/// so the leg's cookie names the call's two owners and its INVITE responses
/// reach the primary while it lives (ADR-0055 §8). Its CANCEL and non-2xx ACK
/// share this Via.
pub fn dialling_via(
    config: &B2buaConfig,
    marks: CallMarks,
    leg_id: &str,
    request: RequestFacts,
    branch: String,
) -> Via {
    build_call_via(&marks.opts(config, leg_id, Some(request), true), branch)
}

/// The Contact URI of the leg `leg_id` of the call `marks` name, allocated
/// when the leg is created and stored on it ([`call::Leg::contact`]): it names
/// [`B2buaConfig::contact_address`], so every copy of the call states the same
/// Contact (ADR-0055 §14).
pub fn allocate_leg_contact(config: &B2buaConfig, marks: ContactMarks, leg_id: &str) -> String {
    let (host, port) = config.contact_address();
    build_call_contact(&ContactFacts {
        host,
        port,
        call_ref: marks.call_ref,
        leg: leg_id,
        incarnation_mark: marks.incarnation_mark,
        is_emergency: marks.is_emergency,
    })
    .text()
    .into_owned()
}

/// What of a call its Contacts carry: fixed when the call is created.
#[derive(Clone, Copy, Debug)]
pub struct ContactMarks<'a> {
    pub call_ref: &'a str,
    /// [`Call::incarnation_mark`].
    pub incarnation_mark: &'a str,
    /// `call.emergency == Some(true)`.
    pub is_emergency: bool,
}

impl<'a> From<CallMarks<'a>> for ContactMarks<'a> {
    fn from(marks: CallMarks<'a>) -> Self {
        Self {
            call_ref: marks.call_ref,
            incarnation_mark: marks.incarnation_mark,
            is_emergency: marks.is_emergency,
        }
    }
}

/// The B2BUA's Contact on `call`'s leg `leg_id`: the one stored on the leg.
/// `None` where the call holds no such leg or its stored Contact is no URI.
pub fn leg_contact(call: &Call, leg_id: &str) -> Option<header::Contact> {
    stated_contact(&call::helpers::find_leg(call, leg_id)?.contact)
}

/// The Contact header stating `contact`, a leg's allocated Contact URI.
pub fn stated_contact(contact: &str) -> Option<header::Contact> {
    Uri::parse(&SipStr::owned(contact)).ok().map(header::Contact::from_uri)
}
