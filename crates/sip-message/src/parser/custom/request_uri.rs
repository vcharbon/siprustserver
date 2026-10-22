//! What a URI must be to stand as a Request-URI: a URI the one reader takes
//! whole ([`Uri::read_exact`]), with a host and no escaped headers (RFC 3261
//! §19.1.1 Table 1). Every request is held to it at its freeze, so a URI that
//! fails it is no request's target, whether a peer sent it in a start line or
//! recorded it as a route (RFC 3261 §12.2.1.1, §16.12).

use crate::error::SipParseError;
use crate::header::{Uri, UriRole};
use crate::sip_str::SipStr;

/// The Request-URI `text` names, or why it names none.
pub(crate) fn read_request_uri(text: &SipStr) -> Result<Uri, SipParseError> {
    let uri = Uri::read_exact(text, UriRole::Target)
        .map_err(|e| SipParseError::new(format!("Malformed Request-URI: {}", e.reason)))?;
    request_uri_refusal(&uri)?;
    Ok(uri)
}

/// Why the URI already read as `uri` cannot stand as a Request-URI, if it
/// cannot.
pub(crate) fn request_uri_refusal(uri: &Uri) -> Result<(), SipParseError> {
    if uri.is_opaque() {
        return Err(SipParseError::new(format!("Malformed Request-URI: \"{uri}\"")));
    }
    if uri.host().is_empty() {
        return Err(SipParseError::new(format!("Empty host in Request-URI: \"{uri}\"")));
    }
    if uri.escaped_headers().next().is_some() {
        return Err(SipParseError::new(format!(
            "Request-URI carries escaped headers (RFC 3261 §19.1.1): \"{uri}\""
        )));
    }
    Ok(())
}
