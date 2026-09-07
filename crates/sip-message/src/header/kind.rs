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
pub trait NameAddrKind: HeaderKind {}

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

/// A header whose value is a set of option tags.
pub trait TokenKind: HeaderKind {}

/// A header whose value is a leading token followed by `;`-parameters.
pub trait TokenParamsKind: HeaderKind {}

/// A header whose value is a single number, bounded by the range its own
/// registry entry states. The bounds are the same ones the header-block parser
/// gates on, so a value this stack builds is one it would also accept.
pub trait NumericKind: HeaderKind {
    /// The smallest value the grammar admits.
    const MIN: u32 = 0;
    /// The largest value the grammar admits.
    const MAX: u32 = u32::MAX;
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

capability! { NameAddrKind:
    From, To, Contact, Route, RecordRoute, Path, ServiceRoute, ReferTo, ReferredBy, ReplyTo,
    Diversion, HistoryInfo, RemotePartyId, PAssertedIdentity, PPreferredIdentity,
}

capability! { RichParams:
    From, To, Contact, Route, RecordRoute, Path, ServiceRoute, ReferTo, ReferredBy, ReplyTo,
    Diversion, HistoryInfo, RemotePartyId,
}

capability! { NoParams: PAssertedIdentity, PPreferredIdentity }

capability! { TaggedKind: From, To }

capability! { TokenKind: Require, ProxyRequire, Supported, Unsupported, Allow, AllowEvents, Privacy }

capability! { TokenParamsKind:
    Event, SubscriptionState, Accept, ContentType, ContentDisposition, SessionExpires, RetryAfter, Reason,
    Replaces,
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
    /// RFC 4028 §5 — `delta-seconds`. The 90-second floor that section states
    /// is a session policy, not a grammar bound, so it is not gated here.
    MinSe => 0, u32::MAX;
    /// RFC 3262 §7.1 — `1` to `2^31 - 1`; zero is not a sequence number.
    RSeq => 1, INT_32_MAX;
}

capability! { CredentialsKind:
    Authorization, ProxyAuthorization, WwwAuthenticate, ProxyAuthenticate,
}
