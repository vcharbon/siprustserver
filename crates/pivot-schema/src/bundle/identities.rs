//! The **user identities** a recorded datagram names: its From and To and, on
//! a request, its Request-URI, each reduced by `sip-message` to the subscriber
//! it names. A reader keys a recorded call on these, never on a URI rule of its
//! own. The shape is the flows document's identity (`sip-pcap` `doc::Identity`),
//! so one mirror reads both.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sip_message::header::Uri;
use sip_message::SipMessage;

/// One URI reduced to the subscriber it names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    /// The URI as the wire spelled it.
    pub uri: String,
    /// [`Uri::user_identity`]: user-parameters dropped, RFC 3966 visual
    /// separators removed on a phone-shaped user; null for a userless URI.
    pub user: Option<String>,
    /// [`Uri::user_digits`]: `user` reduced to digits with one leading `00`/`0`
    /// dropped; null when the identity carries no digit.
    pub digits: Option<String>,
}

impl Identity {
    /// The identity `uri` names.
    pub fn of(uri: &Uri) -> Self {
        Identity {
            uri: uri.text().into_owned(),
            user: uri.user_identity(),
            digits: uri.user_digits(),
        }
    }
}

/// The user identities one datagram names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Identities {
    pub from: Identity,
    pub to: Identity,
    /// The Request-URI's: requests only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ruri: Option<Identity>,
}

impl Identities {
    /// The identities `message` names.
    pub fn of(message: &SipMessage) -> Self {
        Identities {
            from: Identity::of(message.from().uri()),
            to: Identity::of(message.to().uri()),
            ruri: match message {
                SipMessage::Request(request) => Some(Identity::of(request.request_uri())),
                SipMessage::Response(_) => None,
            },
        }
    }
}
