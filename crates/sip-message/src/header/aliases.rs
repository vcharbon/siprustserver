//! The concrete header value types, named after the headers they carry.
//!
//! Each is one instantiation of a generic family, so From and To are distinct
//! types over a single implementation. Refer to them qualified
//! (`header::From`) — a glob import would shadow the prelude's `From` trait.

use super::credentials::Credentials;
use super::kind;
use super::name_addr_header::NameAddrHeader;
use super::numeric::NumericHeader;
use super::token_list::TokenListHeader;
use super::token_params::TokenParamsHeader;

pub type From = NameAddrHeader<kind::From>;
pub type To = NameAddrHeader<kind::To>;
pub type Contact = NameAddrHeader<kind::Contact>;
pub type RouteEntry = NameAddrHeader<kind::Route>;
pub type RecordRouteEntry = NameAddrHeader<kind::RecordRoute>;
pub type PathEntry = NameAddrHeader<kind::Path>;
pub type ServiceRouteEntry = NameAddrHeader<kind::ServiceRoute>;
pub type ReferTo = NameAddrHeader<kind::ReferTo>;
pub type ReferredBy = NameAddrHeader<kind::ReferredBy>;
pub type ReplyTo = NameAddrHeader<kind::ReplyTo>;
pub type Diversion = NameAddrHeader<kind::Diversion>;
pub type HistoryInfo = NameAddrHeader<kind::HistoryInfo>;
pub type RemotePartyId = NameAddrHeader<kind::RemotePartyId>;
pub type PAssertedIdentity = NameAddrHeader<kind::PAssertedIdentity>;
pub type PPreferredIdentity = NameAddrHeader<kind::PPreferredIdentity>;

pub type Require = TokenListHeader<kind::Require>;
pub type ProxyRequire = TokenListHeader<kind::ProxyRequire>;
pub type Supported = TokenListHeader<kind::Supported>;
pub type Unsupported = TokenListHeader<kind::Unsupported>;
pub type Allow = TokenListHeader<kind::Allow>;
pub type AllowEvents = TokenListHeader<kind::AllowEvents>;
/// The RFC 3323 §4.2 priv-value set — a token list whose members its own
/// grammar separates with `;`.
pub type Privacy = TokenListHeader<kind::Privacy>;

pub type Event = TokenParamsHeader<kind::Event>;
pub type SubscriptionState = TokenParamsHeader<kind::SubscriptionState>;
/// RFC 4488 §4 — whether a REFER asks for its implicit subscription:
/// `true` or `false`, then extension parameters.
pub type ReferSub = TokenParamsHeader<kind::ReferSub>;
/// One media range of an `Accept` line (RFC 3261 §20.1); a line carries several,
/// comma-separated. Its leading `type/subtype` is kept as written, SWS around the
/// slash included, unlike [`MediaType`](super::MediaType): the capability relay
/// restates a range as written and classifies none.
pub type AcceptRange = TokenParamsHeader<kind::Accept>;
pub type ContentDisposition = TokenParamsHeader<kind::ContentDisposition>;
pub type SessionExpires = TokenParamsHeader<kind::SessionExpires>;
pub type RetryAfter = TokenParamsHeader<kind::RetryAfter>;
pub type Reason = TokenParamsHeader<kind::Reason>;
/// RFC 3891 — the dialog an INVITE asks to replace: the Call-ID as the leading
/// token, `to-tag` and `from-tag` as parameters.
pub type Replaces = TokenParamsHeader<kind::Replaces>;

pub type MaxForwards = NumericHeader<kind::MaxForwards>;
pub type ContentLength = NumericHeader<kind::ContentLength>;
pub type Expires = NumericHeader<kind::Expires>;
pub type MinExpires = NumericHeader<kind::MinExpires>;
pub type MinSe = NumericHeader<kind::MinSe>;
pub type RSeq = NumericHeader<kind::RSeq>;

pub type Authorization = Credentials<kind::Authorization>;
pub type ProxyAuthorization = Credentials<kind::ProxyAuthorization>;
pub type WwwAuthenticate = Credentials<kind::WwwAuthenticate>;
pub type ProxyAuthenticate = Credentials<kind::ProxyAuthenticate>;
