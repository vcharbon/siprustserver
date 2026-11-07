//! The headers whose grammar is single-valued: a message carries one line of
//! each (RFC 3261 §7.3.1), compact forms counted. The list is stated by hand
//! from each header's ABNF, errata applied; none of them is a comma list. A
//! header not on it may repeat.

use super::id::HeaderKey;
use super::name::{HeaderId, HeaderName};
use crate::error::SipParseError;
use crate::sip_str::SipStr;

/// A header named at compile time: a known one by id, an extension one by its
/// name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Listed {
    Known(HeaderId),
    Extension(&'static str),
}

impl Listed {
    /// The header this names.
    pub fn name(self) -> HeaderName {
        match self {
            Listed::Known(id) => id.name(),
            Listed::Extension(name) => HeaderName::Other(SipStr::from_static(name)),
        }
    }
}

/// One row of the list: the header and the grammar line that makes it single.
#[derive(Debug, Clone, Copy)]
pub struct SingleValued {
    pub header: Listed,
    pub grammar: &'static str,
}

const fn known(id: HeaderId, grammar: &'static str) -> SingleValued {
    SingleValued { header: Listed::Known(id), grammar }
}

/// Every single-valued header the stack knows.
pub const SINGLE_VALUED: &[SingleValued] = &[
    known(HeaderId::From, "RFC 3261 §25.1: From = ( \"From\" / \"f\" ) HCOLON from-spec"),
    known(HeaderId::To, "RFC 3261 §25.1: To = ( \"To\" / \"t\" ) HCOLON ( name-addr / addr-spec ) *( SEMI to-param )"),
    known(HeaderId::CallId, "RFC 3261 §25.1: Call-ID = ( \"Call-ID\" / \"i\" ) HCOLON callid"),
    known(HeaderId::CSeq, "RFC 3261 §25.1: CSeq = \"CSeq\" HCOLON 1*DIGIT LWS Method"),
    known(HeaderId::MaxForwards, "RFC 3261 §25.1: Max-Forwards = \"Max-Forwards\" HCOLON 1*DIGIT"),
    known(HeaderId::ContentLength, "RFC 3261 §25.1: Content-Length = ( \"Content-Length\" / \"l\" ) HCOLON 1*DIGIT"),
    known(HeaderId::Expires, "RFC 3261 §25.1: Expires = \"Expires\" HCOLON delta-seconds"),
    known(HeaderId::MinExpires, "RFC 3261 §25.1: Min-Expires = \"Min-Expires\" HCOLON delta-seconds"),
    known(HeaderId::SessionExpires, "RFC 4028 §4: Session-Expires = ( \"Session-Expires\" / \"x\" ) HCOLON delta-seconds *( SEMI se-params )"),
    known(HeaderId::MinSe, "RFC 4028 §5: Min-SE = \"Min-SE\" HCOLON delta-seconds *( SEMI generic-param )"),
    known(HeaderId::RSeq, "RFC 3262 §7.1: RSeq = \"RSeq\" HCOLON response-num"),
    known(HeaderId::RAck, "RFC 3262 §7.2: RAck = \"RAck\" HCOLON response-num LWS CSeq-num LWS Method"),
    known(HeaderId::Event, "RFC 6665 §8.4: Event = ( \"Event\" / \"o\" ) HCOLON event-type *( SEMI event-param )"),
    known(HeaderId::SubscriptionState, "RFC 6665 §8.4: Subscription-State = \"Subscription-State\" HCOLON substate-value *( SEMI subexp-params )"),
    known(HeaderId::ReferTo, "RFC 3515 §2.1: Refer-To = ( \"Refer-To\" / \"r\" ) HCOLON ( name-addr / addr-spec ) *( SEMI refer-param )"),
    known(HeaderId::ReferredBy, "RFC 3892 §3: Referred-By = ( \"Referred-By\" / \"b\" ) HCOLON referrer-uri *( SEMI ( referredby-id-param / generic-param ) )"),
    known(HeaderId::ReferSub, "RFC 4488 §4: Refer-Sub = \"Refer-Sub\" HCOLON refer-sub-value *( SEMI exten )"),
    known(HeaderId::Replaces, "RFC 3891 §6.1: Replaces = \"Replaces\" HCOLON callid *( SEMI replaces-param )"),
    known(HeaderId::ReplyTo, "RFC 3261 §25.1: Reply-To = \"Reply-To\" HCOLON rplyto-spec"),
    known(HeaderId::RetryAfter, "RFC 3261 §25.1: Retry-After = \"Retry-After\" HCOLON delta-seconds [ comment ] *( SEMI retry-param )"),
    known(HeaderId::ContentType, "RFC 3261 §25.1: Content-Type = ( \"Content-Type\" / \"c\" ) HCOLON media-type"),
    known(HeaderId::ContentDisposition, "RFC 3261 §25.1: Content-Disposition = \"Content-Disposition\" HCOLON disp-type *( SEMI disp-param )"),
    known(HeaderId::ContentTransferEncoding, "RFC 2045 §6.1: encoding := \"Content-Transfer-Encoding\" \":\" mechanism"),
    known(HeaderId::GeolocationError, "RFC 6442 §4.4: Geolocation-Error = \"Geolocation-Error\" HCOLON locationErrorValue"),
    known(HeaderId::GeolocationRouting, "RFC 6442 §4.2: Georouting-header = \"Geolocation-Routing\" HCOLON ( \"yes\" / \"no\" / generic-value )"),
    SingleValued {
        header: Listed::Extension("P-Charging-Vector"),
        grammar: "RFC 7315 §5.6: P-Charging-Vector = \"P-Charging-Vector\" HCOLON icid-value *( SEMI charge-params )",
    },
    known(HeaderId::Subject, "RFC 3261 §25.1: Subject = ( \"Subject\" / \"s\" ) HCOLON [ TEXT-UTF8-TRIM ]"),
    known(HeaderId::Organization, "RFC 3261 §25.1: Organization = \"Organization\" HCOLON [ TEXT-UTF8-TRIM ]"),
    known(HeaderId::Priority, "RFC 3261 §25.1: Priority = \"Priority\" HCOLON priority-value"),
    known(HeaderId::Date, "RFC 3261 §25.1: Date = \"Date\" HCOLON SIP-date"),
    known(HeaderId::Timestamp, "RFC 3261 §25.1: Timestamp = \"Timestamp\" HCOLON 1*( DIGIT ) [ \".\" *( DIGIT ) ] [ LWS delay ]"),
    known(HeaderId::MimeVersion, "RFC 3261 §25.1: MIME-Version = \"MIME-Version\" HCOLON 1*DIGIT \".\" 1*DIGIT"),
    known(HeaderId::Server, "RFC 3261 §25.1: Server = \"Server\" HCOLON server-val *( LWS server-val )"),
    known(HeaderId::UserAgent, "RFC 3261 §25.1: User-Agent = \"User-Agent\" HCOLON server-val *( LWS server-val )"),
];

/// The known listed headers as a presence set of [`HeaderId::bit`]s.
const KNOWN: u128 = {
    let mut set = 0;
    let mut rest = SINGLE_VALUED;
    while let [row, tail @ ..] = rest {
        if let Listed::Known(id) = row.header {
            set |= id.bit();
        }
        rest = tail;
    }
    set
};

/// The one extension header on the list.
const EXTENSION: &str = "P-Charging-Vector";

/// Whether the table lists `header`, at compile time: the check a value
/// type's cardinality meets.
pub(crate) const fn is_listed(header: Listed) -> bool {
    match header {
        Listed::Known(id) => KNOWN & id.bit() != 0,
        Listed::Extension(name) => name.eq_ignore_ascii_case(EXTENSION),
    }
}

/// Whether the header `name` names, in any casing or form, is single-valued.
pub fn is_single_valued(name: &HeaderName) -> bool {
    match name.key() {
        HeaderKey::Known(id) => KNOWN & id.bit() != 0,
        HeaderKey::Extension(wire) => wire.eq_ignore_ascii_case(EXTENSION),
    }
}

/// The refusal of a second line of the single-valued header `name`.
pub(crate) fn second_line(name: &HeaderName) -> SipParseError {
    let shown = crate::error::reason_text::header(name);
    SipParseError::new(format!("Multiple {shown} headers (RFC 3261 §7.3.1 — one value)"))
}

/// The first single-valued header `lines` carry on a second line, each line
/// given as the header it was classified as and its name as written.
pub(crate) fn first_repeated<'a>(
    lines: impl IntoIterator<Item = (Option<HeaderId>, &'a str)>,
) -> Option<HeaderName> {
    let mut met = Met::default();
    lines
        .into_iter()
        .find(|(id, wire)| met.repeated_line(*id, wire))
        .map(|(id, wire)| id.map_or_else(|| HeaderName::Other(SipStr::owned(wire)), HeaderId::name))
}

/// The single-valued headers a walk over a message's lines has met, so a
/// second line of one is caught whatever its form.
#[derive(Debug, Default)]
pub(crate) struct Met {
    /// The known listed headers met, as [`HeaderId::bit`]s.
    known: u128,
    /// Whether the one listed extension header was met.
    extension: bool,
}

impl Met {
    /// Notes a line of `name`; whether one was met before, `false` for a
    /// header that may repeat.
    pub(crate) fn repeated(&mut self, name: &HeaderName) -> bool {
        self.repeated_line(name.key().id(), name.as_wire_str())
    }

    /// [`repeated`](Self::repeated) for a line classified as `id`, its name
    /// written `wire`.
    pub(crate) fn repeated_line(&mut self, id: Option<HeaderId>, wire: &str) -> bool {
        match id {
            Some(id) if KNOWN & id.bit() != 0 => {
                let met = self.known & id.bit() != 0;
                self.known |= id.bit();
                met
            }
            Some(_) => false,
            None if wire.eq_ignore_ascii_case(EXTENSION) => {
                std::mem::replace(&mut self.extension, true)
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_row_is_single_valued_and_nothing_else_is() {
        assert_eq!(SINGLE_VALUED.len(), 34);
        for row in SINGLE_VALUED {
            assert!(is_single_valued(&row.header.name()), "{row:?}");
        }
        let extensions = SINGLE_VALUED
            .iter()
            .filter(|row| matches!(row.header, Listed::Extension(name) if name == EXTENSION));
        assert_eq!(extensions.count(), 1);
        for name in ["Via", "Contact", "Route", "Require", "Allow", "Warning", "X-Note"] {
            assert!(!is_single_valued(&HeaderName::from(name)), "{name}");
        }
    }

    #[test]
    fn a_second_line_is_found_in_any_form() {
        let line = |name: &'static str| (HeaderId::of(name), name);
        let found = first_repeated([line("From"), line("Via"), line("f")]);
        assert_eq!(found, Some(HeaderName::From));
        let charging = [line("P-Charging-Vector"), line("p-charging-vector")];
        let found = first_repeated(charging).map(|n| n.as_wire_str().to_owned());
        assert_eq!(found.as_deref(), Some("p-charging-vector"));
        assert_eq!(first_repeated([line("Via"), line("v"), line("X-A"), line("X-A")]), None);
        assert!(second_line(&HeaderName::Expires).reason.starts_with("Multiple Expires headers"));
    }

    #[test]
    fn a_compact_form_or_another_casing_names_its_header() {
        for name in ["c", "o", "r", "b", "s", "x", "l", "f", "t", "i", "p-charging-vector"] {
            assert!(is_single_valued(&HeaderName::from(name)), "{name}");
        }
    }
}
