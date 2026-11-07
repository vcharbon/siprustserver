//! The kind axis: a zero-sized marker per header, plus the capability traits
//! that make per-header policy a compile-time fact.
//!
//! A generic value type (`NameAddrHeader<K>`, `TokenListHeader<K>`, …) carries
//! its identity, its comma-foldability and its API surface in `K`, so From and
//! To are different types sharing one implementation, `tag()` exists only where
//! a tag exists, and a credentials header can never be comma-split.
//!
//! Refer to these qualified (`kind::From`) — a glob import would shadow the
//! prelude's `From` trait.

use super::name::HeaderName;
use super::name_addr::AddrForm;
use super::params::ParamReading;
pub use super::token_params::Leading;
use super::uri::UriRole;
use super::value::Folding;

/// `2^31 - 1` — the ceiling RFC 3261 puts on the 32-bit signed counters.
const INT_32_MAX: u32 = i32::MAX as u32;

/// A header's compile-time identity.
pub trait HeaderKind: std::fmt::Debug + Clone + Copy + Send + Sync + 'static {
    fn name() -> HeaderName;
    /// How the wire may lay out several values of this header.
    const FOLDING: Folding;
}

/// A header whose value is a name-addr.
pub trait NameAddrKind: HeaderKind {
    /// Where this header's grammar lets its address stand.
    const FORM: AddrForm = AddrForm::NameAddrOrSpec;
    /// What the address's host is for.
    const ROLE: UriRole = UriRole::Target;
    /// The address names a hop a request is sent to, so its URI must name a
    /// host: one of an opaque scheme is refused.
    const NAMES_HOP: bool = false;
    /// The grammar has header parameters after the address. Without them a
    /// bare value is one URI whole (RFC 3325 §9.1).
    const HEADER_PARAMS: bool = true;
    /// A bare value whose `;` comes before its `@` reads its URI up to that
    /// `@` once the strict read failed (a From or To, RFC 3261 §20).
    const BARE_USER_SEMICOLON: bool = false;
    /// A line may be the `*` wildcard alone (RFC 3261 §20.10), which holds no
    /// address: it reads as no value.
    const WILDCARD: bool = false;
}

/// A name-addr header whose grammar admits header parameters. Absent on
/// P-Asserted-Identity and P-Preferred-Identity, whose RFC 3325 grammar is a
/// bare `name-addr / addr-spec`.
pub trait RichParams: NameAddrKind {}

/// A name-addr header whose grammar has NO header parameters — the RFC 3325
/// P-headers, whose value is a bare `name-addr / addr-spec`. Together with
/// [`RichParams`] this covers every [`NameAddrKind`], so a conversion between
/// kinds must state which side of the axis it lands on.
pub trait NoParams: NameAddrKind {}

/// A name-addr header carrying a dialog tag — From and To, and nothing else.
pub trait TaggedKind: RichParams {}

/// A header whose value is a set of tokens.
pub trait TokenKind: HeaderKind {
    /// The grammar admits a value holding no token (RFC 3261 §25.1:
    /// `Supported` and `Allow` bracket their list).
    const MAY_BE_EMPTY: bool = false;
}

/// A header whose value is a leading part followed by `;`-parameters.
pub trait TokenParamsKind: HeaderKind {
    /// What leads the value.
    const LEADING: Leading = Leading::Token;
    /// How the parameters read: strictly, the whole value consumed, unless
    /// the leading part means something whatever follows it.
    const PARAMS: ParamReading = ParamReading::Strict;
    /// The parameters the grammar requires.
    const REQUIRED: &'static [&'static str] = &[];
    /// The grammar admits an empty line (RFC 3261 §25.1 `Accept`).
    const EMPTY_LINE_READS: bool = false;
    /// The leading token compares byte by byte, not ignoring case.
    const EXACT: bool = false;
}

/// A header whose value is a single number, bounded by the range its own
/// registry entry states. The bounds are the same ones the header-block parser
/// gates on, so a value this stack builds is one it would also accept.
pub trait NumericKind: HeaderKind {
    /// The smallest value the grammar admits.
    const MIN: u32 = 0;
    /// The largest value the grammar admits.
    const MAX: u32 = u32::MAX;
    /// The grammar has `*( SEMI generic-param )` after the number (RFC 4028
    /// §5 Min-SE); a reader holds them to it and keeps the number alone.
    const PARAMS: bool = false;
}

/// A header whose value is an authentication scheme plus comma-separated
/// parameters.
pub trait CredentialsKind: HeaderKind {}

macro_rules! kinds {
    ($($(#[$doc:meta])* $marker:ident => $name:ident, $folding:ident;)+) => {
        $(
            $(#[$doc])*
            #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
            pub struct $marker;

            impl HeaderKind for $marker {
                fn name() -> HeaderName { HeaderName::$name }
                const FOLDING: Folding = Folding::$folding;
            }
        )+
    };
}

macro_rules! capability {
    ($trait_name:ident : $($marker:ident),+ $(,)?) => {
        $(impl $trait_name for $marker {})+
    };
}

/// The numeric registry: one closed range per header, cited to the RFC that
/// states it.
macro_rules! numeric_kinds {
    ($($(#[$doc:meta])* $marker:ident => $min:expr, $max:expr;)+) => {
        $(
            $(#[$doc])*
            impl NumericKind for $marker {
                const MIN: u32 = $min;
                const MAX: u32 = $max;
            }
        )+
    };
}

kinds! {
    From => From, Single;
    To => To, Single;
    Contact => Contact, Comma;
    Route => Route, Comma;
    RecordRoute => RecordRoute, Comma;
    Path => Path, Comma;
    ServiceRoute => ServiceRoute, Comma;
    ReferTo => ReferTo, Single;
    ReferredBy => ReferredBy, Single;
    ReplyTo => ReplyTo, Single;
    Diversion => Diversion, Comma;
    HistoryInfo => HistoryInfo, Comma;
    RemotePartyId => RemotePartyId, Comma;
    PAssertedIdentity => PAssertedIdentity, Comma;
    PPreferredIdentity => PPreferredIdentity, Comma;

    Require => Require, SetPerLine;
    ProxyRequire => ProxyRequire, SetPerLine;
    Supported => Supported, SetPerLine;
    Unsupported => Unsupported, SetPerLine;
    Allow => Allow, SetPerLine;
    AllowEvents => AllowEvents, SetPerLine;
    Privacy => Privacy, SetPerLine;

    Event => Event, Single;
    SubscriptionState => SubscriptionState, Single;
    Accept => Accept, Comma;
    ContentType => ContentType, Single;
    ContentDisposition => ContentDisposition, Single;
    SessionExpires => SessionExpires, Single;
    RetryAfter => RetryAfter, Single;
    Reason => Reason, Comma;
    Replaces => Replaces, Single;
    ReferSub => ReferSub, Single;

    MaxForwards => MaxForwards, Single;
    ContentLength => ContentLength, Single;
    Expires => Expires, Single;
    MinExpires => MinExpires, Single;
    MinSe => MinSe, Single;
    RSeq => RSeq, Single;

    Authorization => Authorization, Opaque;
    ProxyAuthorization => ProxyAuthorization, Opaque;
    WwwAuthenticate => WwwAuthenticate, Opaque;
    ProxyAuthenticate => ProxyAuthenticate, Opaque;
}

capability! { NameAddrKind: ReferTo, ReplyTo }

impl NameAddrKind for Contact {
    const WILDCARD: bool = true;
}

/// The headers whose address names a party and is never resolved.
macro_rules! identity_kinds {
    ($($marker:ident),+) => {
        $(impl NameAddrKind for $marker {
            const ROLE: UriRole = UriRole::Identity;
        })+
    };
}

identity_kinds!(ReferredBy, Diversion, HistoryInfo, RemotePartyId);

/// The dialog's two parties: a bare value may hold a `;` before its `@`.
macro_rules! party_kinds {
    ($($marker:ident),+) => {
        $(impl NameAddrKind for $marker {
            const ROLE: UriRole = UriRole::Identity;
            const BARE_USER_SEMICOLON: bool = true;
        })+
    };
}

party_kinds!(From, To);

/// RFC 3325 §9.1: `name-addr / addr-spec` and no header parameters.
macro_rules! asserted_kinds {
    ($($marker:ident),+) => {
        $(impl NameAddrKind for $marker {
            const ROLE: UriRole = UriRole::Identity;
            const HEADER_PARAMS: bool = false;
        })+
    };
}

asserted_kinds!(PAssertedIdentity, PPreferredIdentity);

/// The route family: `route-param = name-addr *( SEMI rr-param )` (RFC 3261
/// §20.30, §20.34; RFC 3327 §4; RFC 3608 §5).
macro_rules! route_kinds {
    ($($marker:ident),+) => {
        $(impl NameAddrKind for $marker {
            const FORM: AddrForm = AddrForm::NameAddrOnly;
            const NAMES_HOP: bool = true;
        })+
    };
}

route_kinds!(Route, RecordRoute, Path, ServiceRoute);

capability! { RichParams:
    From, To, Contact, Route, RecordRoute, Path, ServiceRoute, ReferTo, ReferredBy, ReplyTo,
    Diversion, HistoryInfo, RemotePartyId,
}

capability! { NoParams: PAssertedIdentity, PPreferredIdentity }

capability! { TaggedKind: From, To }

capability! { TokenKind: Require, ProxyRequire, Unsupported, AllowEvents, Privacy }

impl TokenKind for Supported {
    const MAY_BE_EMPTY: bool = true;
}

impl TokenKind for Allow {
    const MAY_BE_EMPTY: bool = true;
}

capability! { TokenParamsKind: SubscriptionState, ContentDisposition }

/// RFC 6665 §8.2.1: the event package compares byte by byte.
impl TokenParamsKind for Event {
    const EXACT: bool = true;
}

impl TokenParamsKind for Accept {
    const LEADING: Leading = Leading::MediaType;
    const EMPTY_LINE_READS: bool = true;
}

impl TokenParamsKind for ContentType {
    const LEADING: Leading = Leading::MediaType;
}

impl TokenParamsKind for SessionExpires {
    const LEADING: Leading = Leading::DeltaSeconds;
}

impl TokenParamsKind for RetryAfter {
    const LEADING: Leading = Leading::DeltaSecondsComment;
}

/// RFC 3326 §2: a cause reads whatever parameter follows it off the grammar.
impl TokenParamsKind for Reason {
    const PARAMS: ParamReading = ParamReading::UpToFirstBad;
}

/// RFC 3891 §6.1: the dialog's Call-ID, then exactly one of each tag.
impl TokenParamsKind for Replaces {
    const LEADING: Leading = Leading::CallId;
    const REQUIRED: &'static [&'static str] = &["to-tag", "from-tag"];
}

/// RFC 4488 §3: `refer-sub-value = "true" / "false"`.
impl TokenParamsKind for ReferSub {
    const LEADING: Leading = Leading::OneOf(&["true", "false"]);
}

numeric_kinds! {
    /// RFC 3261 §20.22 — a hop count; 255 is the ceiling the header-block
    /// parser gates on and the largest value any hop can honour.
    MaxForwards => 0, 255;
    /// RFC 3261 §20.14 — a body length, gated at `2^31 - 1` like CSeq.
    ContentLength => 0, INT_32_MAX;
    /// RFC 3261 §20.19 — `delta-seconds`, a 32-bit count of seconds.
    Expires => 0, u32::MAX;
    /// RFC 3261 §20.23 — `delta-seconds`.
    MinExpires => 0, u32::MAX;

    /// RFC 3262 §7.1 — `response-num`, `1` to `2^32 - 1`; zero is not a
    /// sequence number. §3's `2^31 - 1` bounds the first value a sender
    /// picks, not what a reader takes.
    RSeq => 1, u32::MAX;
}

/// RFC 4028 §5 — `delta-seconds *( SEMI generic-param )`. The 90-second floor
/// that section states is a session policy, not a grammar bound, so it is not
/// gated here.
impl NumericKind for MinSe {
    const PARAMS: bool = true;
}

capability! { CredentialsKind:
    Authorization, ProxyAuthorization, WwwAuthenticate, ProxyAuthenticate,
}
