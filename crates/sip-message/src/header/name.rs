//! [`HeaderName`] — the header identity every reader dispatches on.

use crate::parser::custom::compact_forms::expanded_name;
use crate::sip_str::SipStr;

/// Declares the known-name variants and their canonical wire spellings in one
/// place, so the enum, the name table and the rendering can never disagree.
macro_rules! known_header_names {
    ($($variant:ident => $wire:literal),+ $(,)?) => {
        /// A header's identity. Known names are unit variants — comparing,
        /// matching and hashing a header name costs no string work — and an
        /// extension header carries its wire spelling in [`Other`](Self::Other).
        ///
        /// Resolution is casing- and compact-form-insensitive: `v`, `V` and
        /// `Via` all resolve to [`Via`](Self::Via), so a reader can never miss
        /// a header for having probed the long form only.
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub enum HeaderName {
            $($variant,)+
            /// An extension header, holding the name exactly as it appeared.
            Other(SipStr),
        }

        impl HeaderName {
            /// Every name resolvable to a unit variant, in declaration order.
            pub const KNOWN: &'static [HeaderName] = &[$(HeaderName::$variant,)+];

            /// The canonical wire spelling — the only casing this crate emits.
            pub fn as_wire_str(&self) -> &str {
                match self {
                    $(HeaderName::$variant => $wire,)+
                    HeaderName::Other(name) => name.as_str(),
                }
            }
        }
    };
}

known_header_names! {
    Accept => "Accept",
    AcceptEncoding => "Accept-Encoding",
    AcceptLanguage => "Accept-Language",
    AlertInfo => "Alert-Info",
    Allow => "Allow",
    AllowEvents => "Allow-Events",
    AuthenticationInfo => "Authentication-Info",
    Authorization => "Authorization",
    CallId => "Call-ID",
    CallInfo => "Call-Info",
    Contact => "Contact",
    ContentDisposition => "Content-Disposition",
    ContentEncoding => "Content-Encoding",
    ContentLanguage => "Content-Language",
    ContentLength => "Content-Length",
    ContentType => "Content-Type",
    CSeq => "CSeq",
    Date => "Date",
    Diversion => "Diversion",
    ErrorInfo => "Error-Info",
    Event => "Event",
    Expires => "Expires",
    From => "From",
    Geolocation => "Geolocation",
    GeolocationError => "Geolocation-Error",
    GeolocationRouting => "Geolocation-Routing",
    HistoryInfo => "History-Info",
    InReplyTo => "In-Reply-To",
    MaxForwards => "Max-Forwards",
    MimeVersion => "MIME-Version",
    MinExpires => "Min-Expires",
    MinSe => "Min-SE",
    Organization => "Organization",
    PAccessNetworkInfo => "P-Access-Network-Info",
    PAssertedIdentity => "P-Asserted-Identity",
    PEarlyMedia => "P-Early-Media",
    PPreferredIdentity => "P-Preferred-Identity",
    Path => "Path",
    Priority => "Priority",
    Privacy => "Privacy",
    ProxyAuthenticate => "Proxy-Authenticate",
    ProxyAuthorization => "Proxy-Authorization",
    ProxyRequire => "Proxy-Require",
    RAck => "RAck",
    Reason => "Reason",
    RecordRoute => "Record-Route",
    ReferTo => "Refer-To",
    ReferredBy => "Referred-By",
    RemotePartyId => "Remote-Party-ID",
    Replaces => "Replaces",
    ReplyTo => "Reply-To",
    Require => "Require",
    ResourcePriority => "Resource-Priority",
    RetryAfter => "Retry-After",
    Route => "Route",
    RSeq => "RSeq",
    Server => "Server",
    ServiceRoute => "Service-Route",
    SessionExpires => "Session-Expires",
    Subject => "Subject",
    SubscriptionState => "Subscription-State",
    Supported => "Supported",
    Timestamp => "Timestamp",
    To => "To",
    Unsupported => "Unsupported",
    UserAgent => "User-Agent",
    UserToUser => "User-to-User",
    Via => "Via",
    Warning => "Warning",
    WwwAuthenticate => "WWW-Authenticate",
}

impl HeaderName {
    /// The variant `name` resolves to, or `None` for an extension header.
    /// Allocation-free — the probe a dispatch loop runs per header.
    pub fn known(name: &str) -> Option<HeaderName> {
        let wire = expanded_name(name).as_bytes();
        let first = wire.first()?.to_ascii_lowercase();
        Self::KNOWN
            .iter()
            .find(|candidate| {
                let c = candidate.as_wire_str().as_bytes();
                c.len() == wire.len()
                    && c[0].to_ascii_lowercase() == first
                    && c.eq_ignore_ascii_case(wire)
            })
            .cloned()
    }

    /// The identity of a parsed header name. An extension header shares the
    /// message image with its `SipStr` rather than copying its bytes.
    pub fn of(name: &SipStr) -> HeaderName {
        Self::known(name.as_str()).unwrap_or_else(|| HeaderName::Other(name.clone()))
    }

    /// Whether `wire` — a header name exactly as it appeared — is this name.
    /// Allocation-free, and casing- and compact-form-insensitive, so it is the
    /// probe a header-list scan runs per line.
    pub fn matches(&self, wire: &str) -> bool {
        match Self::known(wire) {
            Some(known) => known == *self,
            None => matches!(self, HeaderName::Other(name) if name.eq_ignore_ascii_case(wire)),
        }
    }
}

impl std::fmt::Display for HeaderName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_wire_str())
    }
}

impl From<&str> for HeaderName {
    fn from(name: &str) -> Self {
        Self::known(name).unwrap_or_else(|| HeaderName::Other(SipStr::owned(name)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_known_name_resolves_from_its_own_wire_spelling() {
        for name in HeaderName::KNOWN {
            assert_eq!(HeaderName::known(name.as_wire_str()).as_ref(), Some(name));
        }
    }

    #[test]
    fn resolution_ignores_casing() {
        assert_eq!(HeaderName::known("CALL-ID"), Some(HeaderName::CallId));
        assert_eq!(HeaderName::known("cseq"), Some(HeaderName::CSeq));
        assert_eq!(HeaderName::known("wWw-AuThEnTiCaTe"), Some(HeaderName::WwwAuthenticate));
    }

    #[test]
    fn compact_forms_resolve_to_the_long_name() {
        assert_eq!(HeaderName::known("v"), Some(HeaderName::Via));
        assert_eq!(HeaderName::known("F"), Some(HeaderName::From));
        assert_eq!(HeaderName::known("t"), Some(HeaderName::To));
        assert_eq!(HeaderName::known("i"), Some(HeaderName::CallId));
        assert_eq!(HeaderName::known("m"), Some(HeaderName::Contact));
        assert_eq!(HeaderName::known("k"), Some(HeaderName::Supported));
        assert_eq!(HeaderName::known("l"), Some(HeaderName::ContentLength));
        assert_eq!(HeaderName::known("c"), Some(HeaderName::ContentType));
        assert_eq!(HeaderName::known("e"), Some(HeaderName::ContentEncoding));
        assert_eq!(HeaderName::known("s"), Some(HeaderName::Subject));
    }

    #[test]
    fn an_extension_header_keeps_its_wire_spelling() {
        assert_eq!(HeaderName::known("X-Overload"), None);
        let name = HeaderName::of(&SipStr::owned("X-Overload"));
        assert_eq!(name, HeaderName::Other(SipStr::owned("X-Overload")));
        assert_eq!(name.as_wire_str(), "X-Overload");
    }

    #[test]
    fn known_names_render_canonically() {
        assert_eq!(HeaderName::CallId.to_string(), "Call-ID");
        assert_eq!(HeaderName::from("V").as_wire_str(), "Via");
    }

    #[test]
    fn empty_name_resolves_to_nothing() {
        assert_eq!(HeaderName::known(""), None);
    }
}
