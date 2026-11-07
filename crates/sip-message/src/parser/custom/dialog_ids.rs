//! The readings of the values that name a dialog: the From / To address and
//! its tag, and the Call-ID (RFC 3261 §25.1).

use crate::char_class::{is_token_char, is_word_char};
use crate::error::SipParseError;
use crate::header::{self, HeaderName, NameAddr, ParamValue, Reading, Uri};
use crate::parser::{Exemption, Exemptions};
use crate::sip_str::SipStr;

/// One From / To value, its URI read as a [`UriRole::Identity`]: on wire bytes
/// the strict name-addr reading, refused whole on any grammar error and on a
/// shape `exemptions` does not name; in `Hydrate` mode the lenient reading,
/// which never refuses.
pub(super) fn read_address(
    value: &SipStr,
    header: HeaderName,
    wire: bool,
    exemptions: &Exemptions,
) -> Result<NameAddr, SipParseError> {
    let grammar = match header {
        HeaderName::To => header::To::grammar(),
        _ => header::From::grammar(),
    };
    let reading = if wire { Reading::Strict } else { Reading::Lenient };
    let refused = |reason: &str| SipParseError::new(format!("Strict {header}: {reason}"));
    let read = NameAddr::read(value, &grammar, reading).map_err(|e| refused(&e.reason))?;
    if wire {
        if read.bare_user_semicolon && !exemptions.holds(&Exemption::BareUserSemicolon) {
            return Err(refused(
                "a bare address holds a `;` before its `@`, which RFC 3261 §20 brackets",
            ));
        }
        uri_repeat_refusal(read.addr.uri(), &header, exemptions)?;
    }
    Ok(read.addr)
}

/// The refusal of a URI of `header` read with a parameter written twice
/// (RFC 3261 §19.1.1), unless `exemptions` names that parameter of that
/// header.
pub(super) fn uri_repeat_refusal(
    uri: &Uri,
    header: &HeaderName,
    exemptions: &Exemptions,
) -> Result<(), SipParseError> {
    match uri.repeated_params().find(|param| !exemptions.repeats(header, param)) {
        None => Ok(()),
        Some(param) => Err(SipParseError::new(format!(
            "Strict {header}: URI parameter {param:?} written twice (RFC 3261 §19.1.1)"
        ))),
    }
}

/// The dialog tag of a From / To read off the wire: `tag-param = "tag" EQUAL
/// token`.
pub(super) fn tag_refusal(addr: &NameAddr, header: &str) -> Result<(), SipParseError> {
    match addr.params().get("tag") {
        None => Ok(()),
        Some(ParamValue::Token(t)) if t.bytes().all(is_token_char) => Ok(()),
        Some(_) => Err(SipParseError::new(format!("Strict {header}: the tag is not a token"))),
    }
}

/// `callid = word [ "@" word ]`.
pub(crate) fn is_call_id(value: &str) -> bool {
    let word = |w: &str| !w.is_empty() && w.bytes().all(is_word_char);
    match value.split_once('@') {
        Some((local, host)) => word(local) && word(host),
        None => word(value),
    }
}

/// How many `tag` header parameters `addr` holds: one at most (§19.3); a
/// lenient reading keeps every one written.
pub(super) fn tag_count(addr: &NameAddr) -> usize {
    addr.params().iter().filter(|(name, _)| name.eq_ignore_ascii_case("tag")).count()
}
