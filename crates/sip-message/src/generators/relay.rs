//! B2BUA relay transparency: which received headers may be carried onto the
//! message minted for the peer leg, the extractor that selects them, and
//! rebuilding a response for the peer leg from snapshotted fields (RFC 3261
//! §16.6 / §12.1.1).

use super::emit;
use crate::draft::{Entry, ResponseDraft};
use crate::header::{self, HeaderClass, HeaderName, MediaType};
use crate::sip_str::SipStr;
use crate::types::{SipHeader, SipResponse};

/// Which message the relayed headers land on — a request the back-to-back UA
/// mints toward the peer, or a response it mints on the peer's transaction.
/// One class of header travels only the response way ([`WITHHELD_ON_REQUEST`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RelayTarget {
    Request,
    Response,
}

/// What the minted message carries where the source had a body. A header that
/// describes a body must not outlive the body it describes, and a replacement
/// is not an absence: the two are distinct outcomes here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceBody {
    /// The source's own body, byte for byte.
    Verbatim,
    /// A body the relay staged in the source's place, filling the same role.
    Replaced,
    /// No body at all.
    Dropped,
}

/// The message being minted, as far as transparency is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RelayScope {
    /// Which side of the transaction the minted message sits on.
    pub target: RelayTarget,
    /// What the minted message carries where the source had a body.
    pub body: SourceBody,
}

impl RelayScope {
    /// A request minted toward the peer, carrying the source's body.
    pub const fn request() -> Self {
        Self { target: RelayTarget::Request, body: SourceBody::Verbatim }
    }

    /// A response minted on the peer's transaction, carrying the source's body.
    pub const fn response() -> Self {
        Self { target: RelayTarget::Response, body: SourceBody::Verbatim }
    }

    /// A response minted on the peer's transaction, `body` stating what it
    /// carries where the source had one.
    pub const fn response_carrying(body: SourceBody) -> Self {
        Self { target: RelayTarget::Response, body }
    }

    /// The same message with the source's body dropped: nothing describing a
    /// body rides, because the minted message has none.
    pub const fn without_source_body(self) -> Self {
        Self { body: SourceBody::Dropped, ..self }
    }

    /// The same message carrying a body the relay staged in the source's
    /// place. The body's role is unchanged, so the header stating that role
    /// still describes it truthfully; the octets are the relay's own.
    pub const fn with_replaced_body(self) -> Self {
        Self { body: SourceBody::Replaced, ..self }
    }
}

/// True iff the generator owns this header rather than copying the peer's (RFC
/// 3261 §16.6): the stack-owned structural set, plus `Content-Type` — the relay
/// emits its own body, so the media type describing it is the relay's to state.
fn relay_owns(name: &str) -> bool {
    HeaderName::class_of(name) == HeaderClass::Structural
        || HeaderName::known(name) == Some(HeaderName::ContentType)
}

/// End-to-end by class, still not the peer's to receive. One entry per class,
/// each naming the invariant it protects:
const WITHHELD: &[HeaderName] = &[
    // RFC 3261 §22: a credential is scoped to the realm that challenged for it,
    // so relaying one offers it for replay in a domain that never challenged.
    HeaderName::Authorization,
    HeaderName::ProxyAuthorization,
    HeaderName::AuthenticationInfo,
    HeaderName::WwwAuthenticate,
    HeaderName::ProxyAuthenticate,
    // RFC 4028 §7: the refresh interval is negotiated per leg, so a relayed one
    // binds this peer to an interval agreed with the other.
    HeaderName::SessionExpires,
    HeaderName::MinSe,
    // RFC 3261 §20.38 / §20.17: both state when THIS message was sent, so they
    // belong to the stack that mints it (§8.2.6.1 answers a Timestamp with an
    // echo, never with the peer's).
    HeaderName::Timestamp,
    HeaderName::Date,
    // RFC 3891 §3 names a dialog by Call-ID and both tags, all of which a
    // back-to-back UA re-mints per leg: a relayed Replaces names a dialog the
    // peer never had (481).
    HeaderName::Replaces,
];

/// True iff `name` states when THIS message was sent (RFC 3261 §20.17 / §20.38),
/// so the stack that mints a message writes its own and a relay never carries a
/// peer's — the [`WITHHELD`] clock-stamp pair, as a name predicate for callers
/// that hold a wire name rather than a message.
pub fn states_send_time(name: &str) -> bool {
    matches!(HeaderName::known(name), Some(HeaderName::Date | HeaderName::Timestamp))
}

/// Additionally withheld from a relayed REQUEST.
const WITHHELD_ON_REQUEST: &[HeaderName] = &[
    // RFC 3261 §20.32 / §20.29: an imposition on whoever receives the request.
    // This stack is the UAS that just accepted it — re-imposing it on the peer
    // fails (420) a call it already agreed to serve. In a response the same
    // names report the negotiation's outcome and stay end-to-end (RFC 3262).
    HeaderName::Require,
    HeaderName::ProxyRequire,
    HeaderName::Unsupported,
    // RFC 3262 §7.2: RAck names the reliable provisional and the INVITE CSeq of
    // the leg it is sent on, both of which the generator restates.
    HeaderName::RAck,
];

/// States what the body is FOR and how a recipient that cannot process it must
/// answer (RFC 3261 §20.11). It describes the body's role rather than its
/// octets, so it rides wherever a body of that role rides — including onto a
/// body the relay staged in the source's place.
const DESCRIBES_BODY_ROLE: &[HeaderName] = &[HeaderName::ContentDisposition];

/// States a property of the body's OCTETS (RFC 3261 §20.12 / §20.15, RFC 2045
/// §4), so it rides only where those octets themselves ride.
const DESCRIBES_BODY_OCTETS: &[HeaderName] =
    &[HeaderName::ContentEncoding, HeaderName::ContentLanguage, HeaderName::MimeVersion];

/// The network's own assertion about who the sender is — the thing RFC 3323
/// privacy suppresses, as opposed to the `From` claim the sender makes.
const ASSERTED_IDENTITY: &[HeaderName] =
    &[HeaderName::PAssertedIdentity, HeaderName::PPreferredIdentity, HeaderName::RemotePartyId];

/// The RFC 3323 §4.2 priv-values that ask an intermediary to suppress the
/// network's assertion before passing the message on: `id` is RFC 3325 §7's
/// own, `header` and `user` are §5.3's broader levels.
const CONCEALING_PRIV_VALUES: &[&str] = &["id", "header", "user"];

/// True iff `headers` carry a privacy request that conceals the asserted
/// identity. The priv-values are separated as the header's own grammar
/// declares ([`HeaderName::item_separator`]), and a value this stack does not
/// model leaves the assertion alone.
fn privacy_conceals_identity(headers: &[SipHeader]) -> bool {
    let separator = HeaderName::Privacy.item_separator();
    headers
        .iter()
        .filter(|hdr| HeaderName::Privacy.matches(&hdr.name))
        .flat_map(|hdr| separator.split(hdr.value.as_str()))
        .any(|value| CONCEALING_PRIV_VALUES.iter().any(|p| value.eq_ignore_ascii_case(p)))
}

/// True iff a header the back-to-back UA received may be carried onto the
/// message it mints for the peer (RFC 3261 §16.6). An extension header is
/// always relayable: the relay never needs to know what a header means, only
/// whether it is one of the classes it must withhold.
pub fn relayable(name: &str, scope: RelayScope) -> bool {
    if relay_owns(name) {
        return false;
    }
    let Some(known) = HeaderName::known(name) else {
        return true;
    };
    if WITHHELD.contains(&known) {
        return false;
    }
    if scope.target == RelayTarget::Request && WITHHELD_ON_REQUEST.contains(&known) {
        return false;
    }
    match scope.body {
        SourceBody::Verbatim => {}
        SourceBody::Replaced if DESCRIBES_BODY_OCTETS.contains(&known) => return false,
        SourceBody::Replaced => {}
        SourceBody::Dropped
            if DESCRIBES_BODY_ROLE.contains(&known) || DESCRIBES_BODY_OCTETS.contains(&known) =>
        {
            return false
        }
        SourceBody::Dropped => {}
    }
    true
}

/// The received headers that ride onto the minted message, in wire order and
/// with every repeat kept — callers pass the result through `extra_headers`, so
/// each reaches the peer with its name spelling and value bytes unchanged.
///
/// RFC 3325 §7 / RFC 3323 §5.3: a message asking for privacy over its identity
/// leaves the network's assertion behind. The instruction itself travels, so
/// the next element still knows what was asked for; the identity it suppresses
/// does not — the two must never cross a leg together.
pub fn relayable_headers(headers: &[SipHeader], scope: RelayScope) -> Vec<SipHeader> {
    let conceal = privacy_conceals_identity(headers);
    headers
        .iter()
        .filter(|hdr| relayable(&hdr.name, scope))
        .filter(|hdr| !conceal || !ASSERTED_IDENTITY.iter().any(|n| n.matches(&hdr.name)))
        .cloned()
        .collect()
}

/// Inputs for [`generate_relayed_response`]. RFC 3261 §8.2.6.2 makes Via /
/// From / To / Call-ID / CSeq echoes of the request being answered, so each is
/// a draft [`Entry`]: a relay holding the peer's own bytes passes
/// [`Entry::raw`] and they are memcpy'd through untouched; a relay holding a
/// parsed value passes [`Entry::typed`] and it renders once.
#[derive(Debug, Clone, Default)]
pub struct GenerateRelayedResponseOpts {
    /// Via lines from the target-facing request, echoed in order. Required.
    pub vias: Vec<Entry>,
    /// From of the request being answered. Required.
    pub from: Option<Entry>,
    /// To of the request being answered, tagged. Required.
    pub to: Option<Entry>,
    /// Call-ID of the request being answered. Required.
    pub call_id: Option<Entry>,
    /// CSeq of the request being answered. Required.
    pub cseq: Option<Entry>,
    pub body: Vec<u8>,
    pub content_type: Option<MediaType>,
    /// Non-structural headers carried through from the source response (§16.6).
    pub transparent_headers: Vec<SipHeader>,
    /// Timestamp of the request being answered, echoed unchanged (§8.2.6.1 /
    /// §20.38). The relay holds the requester's own bytes, so it rides raw.
    pub timestamp: Option<Entry>,
    /// Record-Route headers reflected in received order.
    pub record_routes: Vec<Entry>,
    pub contact: Option<header::Contact>,
}

/// Put an echoed header on the draft, naming the header it must carry so a
/// missing input fails at the freeze gate rather than silently.
fn echo(draft: ResponseDraft, entry: &Option<Entry>, name: HeaderName) -> ResponseDraft {
    match entry {
        Some(entry) => draft.push_entry(entry.clone()),
        None => draft.push_raw(name, SipStr::EMPTY),
    }
}

/// Rebuild a B2BUA-side response for relay to a peer leg (RFC 3261 §16.6 /
/// §12.1.1).
pub fn generate_relayed_response(
    status: u16,
    reason: &str,
    opts: &GenerateRelayedResponseOpts,
) -> SipResponse {
    let mut draft = ResponseDraft::new(status, SipStr::owned(reason));
    for entry in opts.vias.iter().chain(&opts.record_routes) {
        draft = draft.push_entry(entry.clone());
    }
    draft = echo(draft, &opts.from, HeaderName::From);
    draft = echo(draft, &opts.to, HeaderName::To);
    draft = echo(draft, &opts.call_id, HeaderName::CallId);
    draft = echo(draft, &opts.cseq, HeaderName::CSeq);
    if let Some(timestamp) = &opts.timestamp {
        draft = draft.push_entry(timestamp.clone());
    }

    draft = emit::extra_headers(draft, &opts.transparent_headers);

    if let Some(contact) = opts.contact.clone() {
        draft = draft.push(contact);
    }

    emit::response(emit::framed(draft, opts.body.clone(), opts.content_type.clone()))
}

#[cfg(test)]
mod clock_stamp_tests {
    use super::*;

    /// [`states_send_time`] and [`WITHHELD`] name the same clock stamps — the
    /// predicate is the table's name-shaped form, not a second policy.
    #[test]
    fn the_predicate_names_exactly_the_withheld_clock_stamps() {
        for name in ["Date", "date", "Timestamp"] {
            assert!(states_send_time(name), "{name} states send time");
            let known = HeaderName::known(name).expect("a known header");
            assert!(WITHHELD.contains(&known), "{name} is withheld from a relay");
        }
        for name in ["Allow", "Supported", "P-Term", "Content-Disposition"] {
            assert!(!states_send_time(name), "{name} does not state send time");
        }
    }
}
