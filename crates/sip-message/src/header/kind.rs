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

/// A name-addr header carrying a dialog tag — From and To, and nothing else.
pub trait TaggedKind: RichParams {}

/// A header whose value is a set of option tags.
pub trait TokenKind: HeaderKind {}

/// A header whose value is a leading token followed by `;`-parameters.
pub trait TokenParamsKind: HeaderKind {}

/// A header whose value is a single number.
pub trait NumericKind: HeaderKind {}

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

    Require => Require, LinePerValue;
    ProxyRequire => ProxyRequire, LinePerValue;
    Supported => Supported, LinePerValue;
    Unsupported => Unsupported, LinePerValue;
    Allow => Allow, LinePerValue;
    AllowEvents => AllowEvents, LinePerValue;

    Event => Event, Single;
    SubscriptionState => SubscriptionState, Single;
    ContentType => ContentType, Single;
    ContentDisposition => ContentDisposition, Single;
    SessionExpires => SessionExpires, Single;
    RetryAfter => RetryAfter, Single;
    Reason => Reason, Comma;

    MaxForwards => MaxForwards, Single;
    ContentLength => ContentLength, Single;
    Expires => Expires, Single;
    MinExpires => MinExpires, Single;
    MinSe => MinSe, Single;
    RSeq => RSeq, Single;

    Authorization => Authorization, LinePerValue;
    ProxyAuthorization => ProxyAuthorization, LinePerValue;
    WwwAuthenticate => WwwAuthenticate, LinePerValue;
    ProxyAuthenticate => ProxyAuthenticate, LinePerValue;
}

capability! { NameAddrKind:
    From, To, Contact, Route, RecordRoute, Path, ServiceRoute, ReferTo, ReferredBy, ReplyTo,
    Diversion, HistoryInfo, RemotePartyId, PAssertedIdentity, PPreferredIdentity,
}

capability! { RichParams:
    From, To, Contact, Route, RecordRoute, Path, ServiceRoute, ReferTo, ReferredBy, ReplyTo,
    Diversion, HistoryInfo, RemotePartyId,
}

capability! { TaggedKind: From, To }

capability! { TokenKind: Require, ProxyRequire, Supported, Unsupported, Allow, AllowEvents }

capability! { TokenParamsKind:
    Event, SubscriptionState, ContentType, ContentDisposition, SessionExpires, RetryAfter, Reason,
}

capability! { NumericKind: MaxForwards, ContentLength, Expires, MinExpires, MinSe, RSeq }

capability! { CredentialsKind:
    Authorization, ProxyAuthorization, WwwAuthenticate, ProxyAuthenticate,
}
