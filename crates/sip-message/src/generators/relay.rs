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

/// The message being minted, as far as transparency is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RelayScope {
    /// Which side of the transaction the minted message sits on.
    pub target: RelayTarget,
    /// Whether the minted message carries the source's own body — a header
    /// that describes a body must not outlive the body it describes.
    pub carries_source_body: bool,
}

impl RelayScope {
    /// A request minted toward the peer, carrying the source's body.
    pub const fn request() -> Self {
        Self { target: RelayTarget::Request, carries_source_body: true }
    }

    /// A response minted on the peer's transaction, carrying the source's body.
    pub const fn response() -> Self {
        Self { target: RelayTarget::Response, carries_source_body: true }
    }

    /// The same message with the source's body dropped or replaced.
    pub const fn without_source_body(self) -> Self {
        Self { carries_source_body: false, ..self }
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

/// Describes the body, so it rides only where that body rides (RFC 3261 §20.11
/// / §20.12 / §20.15, RFC 2045 §4).
const DESCRIBES_BODY: &[HeaderName] = &[
    HeaderName::ContentDisposition,
    HeaderName::ContentEncoding,
    HeaderName::ContentLanguage,
    HeaderName::MimeVersion,
];

/// The network's own assertion about who the sender is — the thing RFC 3323
/// privacy suppresses, as opposed to the `From` claim the sender makes.
const ASSERTED_IDENTITY: &[HeaderName] = &[
    HeaderName::PAssertedIdentity,
    HeaderName::PPreferredIdentity,
    HeaderName::RemotePartyId,
];

/// The RFC 3323 §4.2 priv-values that ask an intermediary to suppress the
/// network's assertion before passing the message on: `id` is RFC 3325 §7's
/// own, `header` and `user` are §5.3's broader levels.
const CONCEALING_PRIV_VALUES: &[&str] = &["id", "header", "user"];

/// True iff `headers` carry a privacy request that conceals the asserted
/// identity. The priv-values are `;`-separated (RFC 3323 §4.2), and a value
/// this stack does not model leaves the assertion alone.
fn privacy_conceals_identity(headers: &[SipHeader]) -> bool {
    headers
        .iter()
        .filter(|hdr| HeaderName::Privacy.matches(&hdr.name))
        .flat_map(|hdr| hdr.value.as_str().split(';'))
        .any(|value| CONCEALING_PRIV_VALUES.iter().any(|p| value.trim().eq_ignore_ascii_case(p)))
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
    if !scope.carries_source_body && DESCRIBES_BODY.contains(&known) {
        return false;
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
