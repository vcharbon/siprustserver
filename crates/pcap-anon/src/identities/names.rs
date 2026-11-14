//! The names a datagram carries outside its numbers, read through the
//! `sip-message` seam: the host of every Via (with its `received` and
//! `maddr`), the host a Call-ID names, the warn-agent of every Warning, the
//! host of every network URI (`http:`, `https:`) a header value holds, the
//! charging id and the hosts of the charging headers, the realm of every
//! credentials header, the domain of every message id (`cid:`, `mid:`,
//! `Content-ID`), every dotted node name of a header outside the registry
//! (a vendor header's value or parameter) and of a Reason outside its text
//! (a Reason's or a Warning's text is protocol text, never read), the
//! identities a
//! header of package, feature or service names carries among its
//! vocabulary, the device id of every `+sip.instance`, the free text of the
//! product, organisation, network and subject headers, every parameter of
//! `P-Access-Network-Info` (cell and location ids), and the PASSporT of an
//! `Identity` header, redacted whole.
//! Every host is read without its port, which names no one. URI hosts, user
//! parts and parameters are harvested where the URIs are
//! ([`super::harvest`]); body parts in [`super::bodies`]. Addresses are not
//! read here: the anonymiser finds every address literal in the bytes.

use sip_message::header::{
    host_of, kind, message_id_domain, network_host, warn_agents, CallId, Credentials, HeaderName,
    HeaderValue, Params, Via,
};
use sip_message::{SipHeader, SipStr};

/// The Via parameters RFC 3261 §20.42 defines (with `rport`, RFC 3581): the
/// names of the others are extensions, whose values may name an element.
const VIA_PARAMS: &[&str] = &["branch", "received", "maddr", "ttl", "rport"];

/// One datagram's names, in wire order, as written.
#[derive(Debug, Default)]
pub struct Names {
    /// Host names, realms, inter-operator ids and letterless device ids.
    pub hosts: Vec<String>,
    /// Charging ids (`P-Charging-Vector` `icid-value`).
    pub icids: Vec<String>,
    /// Product strings and free text naming an organisation, a network, a
    /// subject, a session, a user, a cell or a cause.
    pub texts: Vec<String>,
    /// Values replaced whole by a same-length redaction (a PASSporT: a kept
    /// signature would confirm a guessed number).
    pub redact: Vec<String>,
}

/// `value` with each quoted string blanked, its quotes kept: the text of a
/// Reason or a Warning is protocol text, never read for names
/// ([`crate::rewrite::protocol_spans`]).
pub(super) fn outside_quotes(value: &str) -> String {
    let mut out = value.to_string();
    for (_, quoted) in super::quoted_all(value) {
        let at = quoted.as_ptr() as usize - value.as_ptr() as usize;
        out.replace_range(at..at + quoted.len(), &" ".repeat(quoted.len()));
    }
    out
}

/// Harvest the names of every header of a header block into `out`.
pub(super) fn harvest(headers: &[SipHeader], out: &mut Names) {
    for h in headers {
        let value = &h.value;
        let text = || value.as_str().trim().to_string();
        match HeaderName::of(&h.name) {
            HeaderName::Via => {
                for via in Via::parse_line(value).unwrap_or_default() {
                    out.hosts.push(via.host().to_string());
                    out.hosts.extend(via.received().map(str::to_string));
                    out.hosts.extend(via.maddr().map(str::to_string));
                    // An extension parameter's dotted node name names an element;
                    // the branch is the identifier pass's.
                    for (name, param) in via.params().iter() {
                        if !VIA_PARAMS.iter().any(|p| p.eq_ignore_ascii_case(name)) {
                            node_names(param.as_str().unwrap_or_default(), out);
                        }
                    }
                }
            }
            HeaderName::CallId => call_id_host(value.as_str(), out),
            // The warn-agent is a field and names an element; the warn-text is
            // protocol text.
            HeaderName::Warning => {
                let agents = warn_agents(value.as_str());
                out.hosts.extend(agents.into_iter().map(|a| host_of(a).to_string()));
            }
            // A dotted token in a protocol slot names no registered protocol;
            // the quoted text is protocol text.
            HeaderName::Reason => node_names(&outside_quotes(value.as_str()), out),
            HeaderName::PAccessNetworkInfo => {
                let params = Params::parse_list(value);
                let values = params.iter().skip(1).filter_map(|(_, v)| v.as_str());
                out.texts.extend(values.map(|v| v.trim_matches('"').to_string()));
            }
            _ if h.name.as_str().eq_ignore_ascii_case("Identity") => {
                let token = value.as_str().split(';').next().unwrap_or_default().trim();
                if !token.is_empty() {
                    out.redact.push(token.to_string());
                }
            }
            HeaderName::Server
            | HeaderName::UserAgent
            | HeaderName::Organization
            | HeaderName::Subject => {
                out.texts.push(text());
            }
            HeaderName::Authorization => realm::<kind::Authorization>(value, out),
            HeaderName::ProxyAuthorization => realm::<kind::ProxyAuthorization>(value, out),
            HeaderName::WwwAuthenticate => realm::<kind::WwwAuthenticate>(value, out),
            HeaderName::ProxyAuthenticate => realm::<kind::ProxyAuthenticate>(value, out),
            _ if h.name.as_str().eq_ignore_ascii_case("P-Charging-Vector") => {
                charging(&Params::parse_list(value), out);
            }
            _ if h.name.as_str().eq_ignore_ascii_case("P-Charging-Function-Addresses") => {
                charging(&Params::parse_list(value), out);
            }
            _ if h.name.as_str().eq_ignore_ascii_case("P-Visited-Network-ID") => {
                out.texts.push(text())
            }
            _ if h.name.as_str().eq_ignore_ascii_case("Content-ID") => {
                out.hosts.extend(message_id_domain(value.as_str()).map(str::to_string));
            }
            // A Session-ID is the identifier pass's.
            _ if h.name.as_str().eq_ignore_ascii_case("Session-ID") => {}
            _ if vocabulary_header(h.name.as_str()) => {
                let media_type = h.name.as_str().eq_ignore_ascii_case("Content-Type");
                vocabulary_values(value.as_str(), media_type, out)
            }
            HeaderName::Other(_) => node_names(value.as_str(), out),
            _ => {}
        }
        network_hosts(value.as_str(), out);
        message_ids(value.as_str(), out);
        instance_ids(value.as_str(), out);
    }
}

/// The device id of every `+sip.instance` parameter a header value holds
/// (RFC 5626 §4.1: a URN in angle brackets), its last `:`-segment: free text,
/// or, without a letter (an IMEI), faked whole like a host.
pub(super) fn instance_ids(value: &str, out: &mut Names) {
    let lower = value.to_ascii_lowercase();
    let mut from = 0;
    while let Some(p) = lower[from..].find("+sip.instance=") {
        let start = from + p + "+sip.instance=".len();
        let end = value[start..].find(['>', ';', ',']).map_or(value.len(), |e| start + e);
        let urn = value[start..end].trim_matches(['"', '<', '>']);
        if let Some(id) = urn.rsplit(':').next().filter(|id| !id.is_empty()) {
            if id.bytes().any(|b| b.is_ascii_alphabetic()) {
                out.texts.push(id.to_string());
            } else {
                out.hosts.push(id.to_string());
            }
        }
        from = end.max(start);
    }
}

/// The host of every network URI (`scheme://host…`) a header value holds.
pub(super) fn network_hosts(value: &str, out: &mut Names) {
    let mut rest = value;
    while let Some(at) = rest.find("://") {
        let start = rest[..at]
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || "+-.".contains(c)))
            .map_or(0, |p| p + 1);
        let end = rest[at..]
            .find(|c: char| c.is_whitespace() || "<>,\"".contains(c))
            .map_or(rest.len(), |p| at + p);
        out.hosts.extend(network_host(&rest[start..end]).map(str::to_string));
        rest = &rest[end.max(at + 3)..];
    }
}

/// The domain of every `cid:` or `mid:` URI a header value holds.
pub(super) fn message_ids(value: &str, out: &mut Names) {
    for token in value.split(|c: char| c.is_whitespace() || "<>,;\"".contains(c)) {
        if token
            .get(..4)
            .is_some_and(|s| s.eq_ignore_ascii_case("cid:") || s.eq_ignore_ascii_case("mid:"))
        {
            out.hosts.extend(message_id_domain(token).map(str::to_string));
        }
    }
}

/// Every dotted name a vendor header's value or parameter holds: two labels
/// or more of letters, digits, hyphens and underscores, the last opening on
/// a letter — the node names and dotted opaque keys a platform writes into
/// headers no registry defines, read as the leak check reads a domain,
/// whatever the length of a label. A percent escape (`%40`) separates two
/// tokens and belongs to neither. A feature tag (behind a `+`) and a URN's
/// namespace string are vocabulary.
pub(super) fn node_names(value: &str, out: &mut Names) {
    let bytes = value.as_bytes();
    for (at, token) in escape_split(value) {
        let word_start = bytes[..at]
            .iter()
            .rposition(|b| b.is_ascii_whitespace() || b"<\"=,;".contains(b))
            .map_or(0, |p| p + 1);
        let urn = bytes[word_start..].get(..4).is_some_and(|s| s.eq_ignore_ascii_case(b"urn:"));
        // Behind a single `/`: a media subtype or a path, never a host (`//` opens one).
        let subtype = at > 0 && bytes[at - 1] == b'/' && !(at > 1 && bytes[at - 2] == b'/');
        if urn || subtype || (at > 0 && bytes[at - 1] == b'+') {
            continue;
        }
        let token = token.trim_matches(['.', '-', '_']);
        let labels: Vec<&str> = token.split('.').collect();
        let named = labels.len() >= 2
            && labels.last().is_some_and(|l| l.starts_with(|c: char| c.is_ascii_alphabetic()))
            && labels.iter().all(|l| {
                !l.is_empty()
                    && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            });
        if named {
            out.hosts.push(token.to_string());
        }
    }
}

/// The identities a header of package, feature or service names carries
/// among that vocabulary: the free text of a quoted parameter value that is
/// no URI and no URN (`+sip.description="…"`), and the node names of a bare
/// value or an unquoted parameter value. A feature tag (behind a `+`), a URN
/// and a name of the global, SIP, 3GPP or vendor tree (`g.3gpp.…`, `vnd.…`) are
/// vocabulary, and so is the media type itself. A
/// media type's quoted parameters and its framing ones (`boundary`, `start`,
/// `charset`, `type`, `version`) are the message's.
fn vocabulary_values(value: &str, media_type: bool, out: &mut Names) {
    const FRAMING: &[&str] = &["boundary", "start", "charset", "type", "version"];
    for element in top_level_split(value) {
        let element = element.trim();
        let (name, val) = element.split_once('=').unwrap_or((element, ""));
        let name = name.trim();
        if val.is_empty() {
            let tree = matches!(name.split('.').next(), Some("g" | "sip" | "3gpp" | "vnd"));
            if !tree && !media_type {
                node_names(name, out);
            }
            continue;
        }
        if media_type && FRAMING.iter().any(|f| f.eq_ignore_ascii_case(name)) {
            continue;
        }
        let val = val.trim();
        match val.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
            Some(inner) => {
                let decoded = sip_message::param_codec::decode_param(inner);
                let vocabulary = inner.starts_with('<')
                    || decoded.get(..4).is_some_and(|s| s.eq_ignore_ascii_case("urn:"));
                if !vocabulary && !media_type && !inner.is_empty() {
                    out.texts.push(inner.to_string());
                }
            }
            None => node_names(val, out),
        }
    }
}

/// The comma- and semicolon-separated elements of a header value, quoted
/// strings kept whole.
fn top_level_split(value: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut start, mut quoted) = (0, false);
    for (i, c) in value.char_indices() {
        match c {
            '"' => quoted = !quoted,
            ',' | ';' if !quoted => {
                out.push(&value[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&value[start..]);
    out
}

/// The runs of letters, digits, `.`, `-` and `_` of `value`, each with where
/// it starts, a percent escape's three bytes splitting them.
fn escape_split(value: &str) -> Vec<(usize, &str)> {
    let bytes = value.as_bytes();
    let token_byte = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_');
    let mut tokens = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        let escape = bytes[i] == b'%'
            && bytes.get(i + 1..i + 3).is_some_and(|h| h.iter().all(u8::is_ascii_hexdigit));
        if escape || !token_byte(bytes[i]) {
            // Between two separators every byte is an ASCII token byte, so a
            // non-empty token starts and ends on a character boundary.
            if start < i {
                tokens.push((start, &value[start..i]));
            }
            i += if escape { 3 } else { 1 };
            start = i;
            continue;
        }
        i += 1;
    }
    if start < bytes.len() {
        tokens.push((start, &value[start..]));
    }
    tokens
}

/// Whether a header outside the registry carries protocol vocabulary (Info
/// Package names, feature tags, service identifiers), never node names.
pub(crate) fn vocabulary_header(name: &str) -> bool {
    const NAMES: &[&str] = &[
        "Recv-Info",
        "Info-Package",
        "Feature-Caps",
        "Accept-Contact",
        "Reject-Contact",
        "Request-Disposition",
        "P-Asserted-Service",
        "P-Preferred-Service",
        "Session-ID",
        "Security-Client",
        "Security-Server",
        "Security-Verify",
        "Content-Type",
        "Identity",
    ];
    NAMES.iter().any(|n| n.eq_ignore_ascii_case(name))
}

/// The host a Call-ID (or a `Replaces` dialog id) names, without a port: its
/// part behind the `@` where that reads as a host.
pub(super) fn call_id_host(value: &str, out: &mut Names) {
    let call_id = value.split(';').next().unwrap_or_default();
    let parsed = CallId::parse(&sip_message::SipStr::owned(call_id.trim())).ok();
    let host = parsed.and_then(|c| c.host().map(|h| host_of(h).to_string()));
    // An undotted part is another identifier, the identifier pass's.
    out.hosts.extend(host.filter(|h| crate::rewrite::identifiers::is_host_word(h.as_bytes())));
}

/// The realm a credentials header names.
fn realm<K: kind::CredentialsKind>(value: &SipStr, out: &mut Names) {
    if let Some(realm) =
        Credentials::<K>::parse(value).ok().and_then(|c| c.value("realm").map(str::to_string))
    {
        out.hosts.push(realm);
    }
}

/// The charging id and the hosts of a charging header's parameters (RFC 7315
/// §5.5, §5.6): the element that minted the id, the inter-operator ids and
/// the charging functions.
fn charging(params: &Params, out: &mut Names) {
    for (name, value) in params.iter() {
        let Some(value) = value.as_str() else { continue };
        let value = value.trim_matches('"');
        match name.to_ascii_lowercase().as_str() {
            "icid-value" => out.icids.push(value.to_string()),
            "icid-generated-at" | "orig-ioi" | "term-ioi" | "ccf" | "ecf" => {
                out.hosts.push(value.to_string());
            }
            _ => {}
        }
    }
}
