//! B2BUA relay transparency: which received headers may be carried onto the
//! message minted for the peer leg, the extractor that selects them, and
//! rebuilding a response for the peer leg from snapshotted fields (RFC 3261
//! §16.6 / §12.1.1).

use super::emit;
use super::relay_policy::{RelayPolicy, RelaySituation};
use crate::draft::{Entry, ResponseDraft};
use crate::header::{self, HeaderClass, HeaderName, HeaderValue, MediaType};
use crate::method::Method;
use crate::sip_str::SipStr;
use crate::types::{SipHeader, SipRequest, SipResponse};

/// Which message the relayed headers land on — a request the back-to-back UA
/// mints toward the peer, or a response it mints on the peer's transaction.
/// One class of header travels only the response way (`WITHHELD_ON_REQUEST`).
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

/// When the minted message leaves, relative to the source it copies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceMoment {
    /// Minted as the source arrives, to carry it on.
    Current,
    /// Minted later from a stored copy of the source: a leg dialled after the
    /// exchange the source opened completed.
    Past,
}

/// Whether the minting element acts as the privacy service toward the
/// receiver of the minted message (RFC 3323 §5, RFC 3325 §7). That role sits at
/// the trust boundary, and the deployment states whether this element holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum IdentityPrivacy {
    /// The element is the privacy service: a message asking for privacy over
    /// its identity leaves the asserted identity behind.
    #[default]
    Conceal,
    /// The element sits inside the trust domain and the next hop is the
    /// boundary: the asserted identity rides beside the privacy request, so the
    /// boundary can act on both (RFC 3325 §5).
    Relay,
}

/// The message being minted, as far as transparency is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RelayScope<'p> {
    /// Which side of the transaction the minted message sits on.
    pub target: RelayTarget,
    /// What the minted message carries where the source had a body.
    pub body: SourceBody,
    /// When the minted message leaves, relative to the source.
    pub moment: SourceMoment,
    /// Whether the minting element conceals the asserted identity.
    pub identity: IdentityPrivacy,
    /// The deployment's relay policy and the message it is read for; `None`
    /// where no policy applies (the originated INVITE).
    pub policy: Option<(&'p RelayPolicy, RelaySituation<'p>)>,
    /// The `Timestamp` the minted message states where the source stated one
    /// ([`RelayScope::stamped`]); `None` states none.
    pub stamp: Option<&'p str>,
}

impl RelayScope<'static> {
    /// A request minted toward the peer, carrying the source's body.
    pub const fn request() -> Self {
        Self {
            target: RelayTarget::Request,
            body: SourceBody::Verbatim,
            moment: SourceMoment::Current,
            identity: IdentityPrivacy::Conceal,
            policy: None,
            stamp: None,
        }
    }

    /// A response minted on the peer's transaction, carrying the source's body.
    pub const fn response() -> Self {
        Self::response_carrying(SourceBody::Verbatim)
    }

    /// A response minted on the peer's transaction, `body` stating what it
    /// carries where the source had one.
    pub const fn response_carrying(body: SourceBody) -> Self {
        Self {
            target: RelayTarget::Response,
            body,
            moment: SourceMoment::Current,
            identity: IdentityPrivacy::Conceal,
            policy: None,
            stamp: None,
        }
    }
}

impl<'p> RelayScope<'p> {
    /// The same message minted by an element whose privacy-service role is
    /// `identity`.
    pub const fn with_identity(self, identity: IdentityPrivacy) -> Self {
        Self { identity, ..self }
    }

    /// The same message minted later from a stored copy of the source: the
    /// source's clock stamps name a moment the minted message does not leave
    /// at, so none rides (RFC 3261 §20.17 / §20.38).
    pub const fn from_stored_copy(self) -> Self {
        Self { moment: SourceMoment::Past, ..self }
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

    /// The same message under the deployment's relay `policy`, read for
    /// `situation`: a header an entry names for that message stays behind.
    pub const fn under<'q>(
        self,
        policy: &'q RelayPolicy,
        situation: RelaySituation<'q>,
    ) -> RelayScope<'q>
    where
        'p: 'q,
    {
        RelayScope {
            target: self.target,
            body: self.body,
            moment: self.moment,
            identity: self.identity,
            policy: Some((policy, situation)),
            stamp: self.stamp,
        }
    }

    /// The same message stating `stamp` as its `Timestamp` where the source
    /// stated one (RFC 3261 §20.38): a request states the minting element's own
    /// clock ([`timestamp_value`]), a response the value of the request it
    /// answers on its own side, echoed with no delay (§8.2.6.1). `None` states
    /// none: a response whose request carried none.
    pub const fn stamped<'q>(self, stamp: Option<&'q str>) -> RelayScope<'q>
    where
        'p: 'q,
    {
        RelayScope {
            target: self.target,
            body: self.body,
            moment: self.moment,
            identity: self.identity,
            policy: self.policy,
            stamp,
        }
    }

    /// True iff the policy this scope is read under leaves `name` behind.
    fn policy_drops(&self, name: &str) -> bool {
        self.policy.is_some_and(|(policy, situation)| policy.drops(name, situation))
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
    // RFC 3891 §3 names a dialog by Call-ID and both tags, all of which a
    // back-to-back UA re-mints per leg: a relayed Replaces names a dialog the
    // peer never had (481).
    HeaderName::Replaces,
];

/// True iff `name` states when the message carrying it was sent (RFC 3261
/// §20.17 / §20.38): a clock reading of whoever sent that message. A minted
/// message from a stored copy ([`RelayScope::from_stored_copy`]) carries
/// neither. Otherwise `Date` rides as the peer wrote it, and `Timestamp` is
/// restated where the source stated one ([`RelayScope::stamped`]).
pub fn states_send_time(name: &str) -> bool {
    matches!(HeaderName::known(name), Some(HeaderName::Date | HeaderName::Timestamp))
}

/// A `Timestamp` value (RFC 3261 §20.38, `1*DIGIT [ "." *DIGIT ]`) reading
/// `now_ms`, milliseconds of the minting element's clock, in seconds.
pub fn timestamp_value(now_ms: i64) -> String {
    format!("{}.{:03}", now_ms.div_euclid(1000), now_ms.rem_euclid(1000))
}

/// Additionally withheld from a relayed REQUEST.
const WITHHELD_ON_REQUEST: &[HeaderName] = &[
    // RFC 3261 §20.32 / §20.29: an imposition on whoever receives the request.
    // This stack is the UAS that just accepted it — re-imposing it on the peer
    // fails (420) a call it already agreed to serve. In a response the same
    // names report the negotiation's outcome and stay end-to-end (RFC 3262).
    // A session request's `Require` keeps its end-to-end tags
    // ([`relayable_request_headers`]).
    HeaderName::Require,
    HeaderName::ProxyRequire,
    HeaderName::Unsupported,
    // RFC 3262 §7.2: RAck names the reliable provisional and the INVITE CSeq of
    // the leg it is sent on, both of which the generator restates.
    HeaderName::RAck,
];

/// The option tags of a session request's `Require` that ride to the peer:
/// extensions whose headers and messages this stack relays end to end without
/// taking part, so the peer is the UAS the requirement is put to. RFC 4028
/// §7.1: `timer` asks the UAS to support the session timer, whose negotiation
/// and refreshes cross the back-to-back UA unchanged.
const END_TO_END_OPTION_TAGS: &[&str] = &["timer"];

/// True iff `method` negotiates the session, so a requirement on its UAS
/// concerns the session's extensions: the INVITE and the two requests that
/// refresh it (RFC 4028 §7.4: re-INVITE, UPDATE).
fn negotiates_session(method: &Method) -> bool {
    matches!(method, Method::Invite | Method::Update)
}

/// A request's `Require` line as it rides to the peer: only its
/// [`END_TO_END_OPTION_TAGS`], verbatim when it names nothing else, `None`
/// when it names none of them.
fn end_to_end_requirement(hdr: &SipHeader) -> Option<SipHeader> {
    let required = header::Require::parse(&hdr.value).ok()?;
    let kept: Vec<&str> = required
        .iter()
        .filter(|tag| END_TO_END_OPTION_TAGS.iter().any(|e| tag.eq_ignore_ascii_case(e)))
        .collect();
    if kept.is_empty() {
        return None;
    }
    if kept.len() == required.len() {
        return Some(hdr.clone());
    }
    Some(SipHeader { name: hdr.name.clone(), value: header::Require::of(kept).to_wire().into() })
}

/// States what the body is FOR and how a recipient that cannot process it must
/// answer (RFC 3261 §20.11). It describes the body's role rather than its
/// octets, so it rides wherever a body of that role rides — including onto a
/// body the relay staged in the source's place.
const DESCRIBES_BODY_ROLE: &[HeaderName] = &[HeaderName::ContentDisposition];

/// States a property of the body's OCTETS (RFC 3261 §20.12 / §20.15, RFC 2045
/// §4 / §6), so it rides only where those octets themselves ride.
const DESCRIBES_BODY_OCTETS: &[HeaderName] = &[
    HeaderName::ContentEncoding,
    HeaderName::ContentLanguage,
    HeaderName::MimeVersion,
    HeaderName::ContentTransferEncoding,
];

/// True iff a header of this name describes the body of the message it heads
/// as the relay classes it: its role (`Content-Disposition`) or its octets
/// (`Content-Encoding`, `Content-Language`, `MIME-Version`,
/// `Content-Transfer-Encoding`), each riding as far as [`relayable`] says.
pub fn describes_body(name: &str) -> bool {
    HeaderName::known(name).is_some_and(|known| {
        DESCRIBES_BODY_ROLE.contains(&known) || DESCRIBES_BODY_OCTETS.contains(&known)
    })
}

/// The lines of `headers` that describe the body of the message they head, in
/// wire order, repeats kept: its role and its octets (RFC 3261 §20.11 /
/// §20.12 / §20.15, RFC 2045 §4 / §6). A body taken whole from that message
/// and sent on another carries them along.
pub fn body_descriptors(headers: &[SipHeader]) -> Vec<SipHeader> {
    headers.iter().filter(|h| describes_body(&h.name)).cloned().collect()
}

/// The media type the message `headers` head states for its body (RFC 3261
/// §20.15), as its sender wrote it.
pub fn body_media_type(headers: &[SipHeader]) -> Option<&SipStr> {
    headers.iter().find(|h| HeaderName::ContentType.matches(&h.name)).map(|h| &h.value)
}

/// The `Timestamp` value among a message's `(name, value)` lines (RFC 3261
/// §20.38), the first one stated: what a response to that request echoes.
pub fn stated_timestamp<'a>(
    lines: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Option<&'a str> {
    lines.into_iter().find(|(name, _)| HeaderName::Timestamp.matches(name)).map(|(_, v)| v)
}

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

/// True iff `scope` makes this element the privacy service and the source
/// `headers` ask it to conceal the asserted identity.
fn conceals_identity(headers: &[SipHeader], scope: RelayScope<'_>) -> bool {
    scope.identity == IdentityPrivacy::Conceal && privacy_conceals_identity(headers)
}

/// True iff `name` is one of the [`ASSERTED_IDENTITY`] headers.
fn asserts_identity(name: &str) -> bool {
    ASSERTED_IDENTITY.iter().any(|n| n.matches(name))
}

/// True iff a header of this name may be carried verbatim onto the message the
/// back-to-back UA mints for the peer, as far as the name decides (RFC 3261
/// §16.6). An extension header passes unless the deployment's relay policy
/// names it for this message: the relay never needs to know what a header
/// means, only whether it is one of the classes it must withhold. The
/// source's own privacy request can still withhold it ([`relayable_from`]).
/// `Timestamp` is never carried verbatim: [`relayable_headers`] restates it.
pub fn relayable(name: &str, scope: RelayScope<'_>) -> bool {
    if relay_owns(name) || scope.policy_drops(name) {
        return false;
    }
    let Some(known) = HeaderName::known(name) else {
        return true;
    };
    if known == HeaderName::Timestamp || WITHHELD.contains(&known) {
        return false;
    }
    if scope.target == RelayTarget::Request && WITHHELD_ON_REQUEST.contains(&known) {
        return false;
    }
    if scope.moment == SourceMoment::Past && states_send_time(name) {
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

/// True iff a header of this name, received among `headers`, may be carried
/// verbatim onto the minted message: [`relayable`], and not an asserted
/// identity the source's privacy request conceals where `scope` makes this
/// element the privacy service (RFC 3325 §7).
pub fn relayable_from(name: &str, headers: &[SipHeader], scope: RelayScope<'_>) -> bool {
    relayable(name, scope) && !(asserts_identity(name) && conceals_identity(headers, scope))
}

/// The received headers that ride onto the minted message, in wire order and
/// with every repeat kept — callers pass the result through `extra_headers`, so
/// each reaches the peer with its name spelling and value bytes unchanged.
/// The one exception is `Timestamp`, a reading of its sender's clock: where
/// the source states one, the minted message states the scope's own
/// ([`RelayScope::stamped`]), once, or none.
///
/// RFC 3325 §7 / RFC 3323 §5.3: where the scope makes this element the
/// privacy service ([`IdentityPrivacy::Conceal`]), a message asking for privacy
/// over its identity leaves the network's assertion behind. The instruction
/// itself always travels, so the next element still knows what was asked for.
pub fn relayable_headers(headers: &[SipHeader], scope: RelayScope<'_>) -> Vec<SipHeader> {
    relayed(headers, scope, false)
}

/// [`relayable_headers`] of a received request, for the request minted from it.
/// A request that negotiates the session also carries its `Require` narrowed
/// to the tags the peer must judge, in place: `timer` (RFC 4028 §7.1).
pub fn relayable_request_headers(request: &SipRequest, scope: RelayScope<'_>) -> Vec<SipHeader> {
    let narrowed_require =
        scope.target == RelayTarget::Request && negotiates_session(request.method());
    relayed(request.headers(), scope, narrowed_require)
}

/// The shared body of [`relayable_headers`] and [`relayable_request_headers`].
fn relayed(headers: &[SipHeader], scope: RelayScope<'_>, narrowed_require: bool) -> Vec<SipHeader> {
    let conceal = conceals_identity(headers, scope);
    let mut stamp = scope.stamp.filter(|_| scope.moment == SourceMoment::Current);
    headers
        .iter()
        .filter(|hdr| !conceal || !asserts_identity(&hdr.name))
        .filter(|hdr| !scope.policy_drops(&hdr.name))
        .filter_map(|hdr| {
            if HeaderName::Timestamp.matches(&hdr.name) {
                stamp
                    .take()
                    .map(|value| SipHeader { name: hdr.name.clone(), value: SipStr::owned(value) })
            } else if relayable(&hdr.name, scope) {
                Some(hdr.clone())
            } else if narrowed_require && HeaderName::Require.matches(&hdr.name) {
                end_to_end_requirement(hdr)
            } else {
                None
            }
        })
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
    draft = emit::extra_headers(draft, &opts.transparent_headers);

    if let Some(contact) = opts.contact.clone() {
        draft = draft.push(contact);
    }

    emit::response(emit::framed(draft, opts.body.clone(), opts.content_type.clone()))
}

#[cfg(test)]
mod clock_stamp_tests {
    use super::*;

    /// [`states_send_time`] names the two clock stamps of RFC 3261 and nothing
    /// else, in any casing.
    #[test]
    fn the_predicate_names_exactly_the_clock_stamps() {
        for name in ["Date", "date", "Timestamp"] {
            assert!(states_send_time(name), "{name} states send time");
        }
        for name in
            ["Allow", "Supported", "X-Vendor-Term", "Content-Disposition", "Session-Expires"]
        {
            assert!(!states_send_time(name), "{name} does not state send time");
        }
    }
}
