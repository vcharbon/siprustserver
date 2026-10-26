//! The header model of ADR-0025: header identity and header values as types.
//!
//! A header's name is a [`HeaderName`], resolved once from the wire bytes —
//! RFC 3261 §7.3.3 compact forms and arbitrary casing collapse into the same
//! variant, and an extension header keeps its wire spelling in
//! [`HeaderName::Other`]. [`HeaderClass`] is the crate's single
//! structural/end-to-end classification: every "which headers does the stack
//! own" table is a query against it.
//!
//! A header's *value* is a [`HeaderValue`]: one type carrying both `parse` and
//! `render`, so a value is its own builder and there is no second shape to keep
//! aligned. The families are generic over a [`kind`] marker that declares the
//! header's identity, its comma-foldability and its API surface — `tag()`
//! exists only on From and To, parameters only where the grammar has them.
//!
//! Refer to the value types qualified (`header::From`) — a glob import of this
//! module would shadow the prelude's `From` trait.

mod aliases;
mod authority;
mod charging;
mod class;
mod credentials;
mod equivalence;
mod geolocation;
mod id;
mod identity;
mod item_separator;
pub mod kind;
mod name;
mod name_addr;
mod name_addr_header;
mod numeric;
mod params;
mod reason;
mod restatement;
mod retry_after;
mod scan;
mod sent_by;
mod token_list;
mod token_params;
mod uri;
mod value;
mod via;
mod warning;
mod wire;

pub use aliases::{
    AcceptRange, Allow, AllowEvents, Authorization, Contact, ContentDisposition, ContentLength,
    Diversion, Event, Expires, From, HistoryInfo, MaxForwards, MediaType, MinExpires, MinSe,
    PAssertedIdentity, PPreferredIdentity, PathEntry, Privacy, ProxyAuthenticate,
    ProxyAuthorization, ProxyRequire, RSeq, Reason, RecordRouteEntry, ReferSub, ReferTo,
    ReferredBy, RemotePartyId, Replaces, ReplyTo, Require, RetryAfter, RouteEntry,
    ServiceRouteEntry, SessionExpires, SubscriptionState, Supported, To, Unsupported,
    WwwAuthenticate,
};
pub use authority::{host_of, message_id_domain, network_host};
pub use charging::ChargingVector;
pub use class::HeaderClass;
pub use credentials::Credentials;
pub use equivalence::{canonical_header_items, header_forms_equivalent};
pub use geolocation::{Geolocation, GeolocationError, GeolocationRouting};
pub use id::HeaderKey;
pub use identity::{CSeq, CallId, RAck};
pub use item_separator::ItemSeparator;
pub use name::{HeaderId, HeaderName};
pub use name_addr::{AddrForm, NameAddr};
pub use name_addr_header::NameAddrHeader;
pub use numeric::NumericHeader;
pub use params::{ParamValue, Params};
pub use reason::{cause_key, q850_cause_alone, readable_reasons, reason_for, Q850};
pub use restatement::Restatement;
pub use retry_after::retry_after_seconds;
pub use sent_by::{SentBy, SentByRef};
pub use token_list::TokenListHeader;
pub use token_params::{parse_token_params, TokenParamsHeader};
pub(crate) use uri::read_sent_by;
pub use uri::{HostPort, Uri, UriRole};
pub use value::{Folding, HeaderValue};
pub use via::{Rport, Via, BRANCH_MAGIC_COOKIE};
pub use warning::{warn_agents, warning_value, WARN_TEXT_MAX};
pub use wire::Wire;
