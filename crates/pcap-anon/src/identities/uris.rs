//! The URIs of a datagram's headers, read through the `sip-message` seam:
//! each address-header line under its own kind, every other header value by
//! a scan for `sip:`/`sips:`/`tel:` URIs, and from each URI its user part (a
//! number or a name), its host, the node names of its parameters and its
//! escaped headers.

use sip_message::header::{kind, HeaderValue, NameAddr, NameAddrHeader, Uri};
use sip_message::parser::custom::structured_headers::split_top_level_commas;
use sip_message::SipStr;

use super::{names, uui_of, Harvested};
use crate::fake::address::ipv4_octets;

/// Harvest every `sip:`/`sips:`/`tel:` URI a header value carries, wherever it
/// sits (bracketed or bare, first or behind a parameter), through the seam's
/// URI reader: a bracketed URI runs to its `>`, a bare one to the next
/// whitespace, `,`, `"`, `<` or `>`. A slice the reader rejects yields nothing.
pub(super) fn scan_uris(value: &SipStr, out: &mut Harvested) {
    let text = value.as_str();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let Some(scheme_len) = uri_scheme_at(bytes, i) else {
            i += 1;
            continue;
        };
        let bracketed = i > 0 && bytes[i - 1] == b'<';
        let end = bytes[i + scheme_len..]
            .iter()
            .position(|b| {
                if bracketed {
                    *b == b'>'
                } else {
                    b.is_ascii_whitespace() || matches!(b, b',' | b'"' | b'<' | b'>')
                }
            })
            .map_or(bytes.len(), |p| i + scheme_len + p);
        if let Ok(uri) = Uri::parse(&value.reslice(&text[i..end])) {
            push_uri(out, &uri);
        }
        i = end.max(i + scheme_len);
    }
}

/// The width of the `sip:`/`sips:`/`tel:` scheme starting at `i`, when one
/// does at a token boundary (the byte before is not alphanumeric).
fn uri_scheme_at(bytes: &[u8], i: usize) -> Option<usize> {
    if i > 0 && bytes[i - 1].is_ascii_alphanumeric() {
        return None;
    }
    [&b"sips:"[..], b"sip:", b"tel:"]
        .into_iter()
        .find(|scheme| {
            bytes[i..].len() >= scheme.len()
                && bytes[i..i + scheme.len()].eq_ignore_ascii_case(scheme)
        })
        .map(<[u8]>::len)
}

/// Harvest one address-header line under its own kind: every address it carries
/// contributes its display name, as written (a quoted one keeps its escapes),
/// and its number.
///
/// An address that yields no number while carrying header parameters may be an
/// unbracketed addr-spec whose userinfo holds an escaped `;` (RFC 3261 §19.1.1):
/// the name-addr grammar cuts it there and the identity would ride through
/// anonymization un-rewritten, so THAT address's own comma entry is ALSO read as
/// a bare addr-spec, which keeps the userinfo whole. The extra read runs per
/// address, so a sibling address on the same line is never harvested twice.
/// A line the name-addr grammar refuses (such an addr-spec carries a `;` §20
/// puts in brackets) is read entry by entry ([`harvest_entry`]).
pub(super) fn harvest_line<K: kind::NameAddrKind>(value: &SipStr, out: &mut Harvested) {
    let entries = split_top_level_commas(value.as_str());
    let Ok(addresses) = NameAddrHeader::<K>::parse_line(value) else {
        for entry in &entries {
            harvest_entry::<K>(&value.reslice(entry), out);
        }
        return;
    };
    // The re-read needs the offending address's own source text: only a line
    // whose entries correspond one-to-one with its addresses has one per
    // address (a comma-folding kind always does; a single-folding kind does
    // unless the line carries a top-level comma the reader did not split on).
    let per_address = entries.len() == addresses.len();
    for (index, address) in addresses.iter().enumerate() {
        if let Some(display) = address.display().filter(|d| !d.is_empty()) {
            let written = per_address
                .then(|| NameAddr::display_as_written(&value.reslice(entries[index])))
                .flatten()
                .map(|d| unquoted(d.as_str()).to_string());
            out.displays.push(written.unwrap_or_else(|| display.to_string()).into_bytes());
        }
        let before = out.numbers.len();
        push_uri(out, address.uri());
        if out.numbers.len() != before || address.addr().params().is_empty() || !per_address {
            continue;
        }
        if let Ok(uri) = Uri::parse(&value.reslice(entries[index])) {
            push_uri_number(out, &uri);
        }
    }
}

/// Harvest one comma entry of a line the name-addr grammar refuses: the entry
/// read as an address of its own, else as a bare URI (the userinfo kept whole)
/// harvested like any other: number, host, parameters and escaped headers.
/// An entry no reader accepts contributes nothing.
fn harvest_entry<K: kind::NameAddrKind>(entry: &SipStr, out: &mut Harvested) {
    match NameAddrHeader::<K>::parse(entry) {
        Ok(address) => {
            if let Some(display) = address.display().filter(|d| !d.is_empty()) {
                let written = NameAddr::display_as_written(entry)
                    .map_or_else(|| display.to_string(), |d| unquoted(d.as_str()).to_string());
                out.displays.push(written.into_bytes());
            }
            push_uri(out, address.uri());
        }
        Err(_) => {
            if let Ok(uri) = Uri::parse(entry) {
                push_uri(out, &uri);
            }
        }
    }
}

/// A display name as written, its enclosing quotes dropped and its escapes
/// kept: the bytes the datagram carries between the quotes.
fn unquoted(written: &str) -> &str {
    written.strip_prefix('"').and_then(|w| w.strip_suffix('"')).unwrap_or(written)
}

/// Harvest a URI: its user part ([`push_uri_number`]), its host or tel
/// `phone-context` domain, every dotted node name a parameter value
/// holds (a trunk group names a network), and its escaped headers decoded —
/// a User-to-User payload, a `Replaces`
/// dialog's Call-ID host, and every URI they hold, read as header values.
pub(super) fn push_uri(out: &mut Harvested, uri: &Uri) {
    push_uri_number(out, uri);
    match uri.scheme() {
        "sip" | "sips" => out.names.hosts.push(uri.host().to_string()),
        // A domain-name `phone-context` (RFC 3966 §5.1.5) names a network; a
        // global-number one is digits the number pass reads.
        "tel" => out.names.hosts.extend(
            uri.param("phone-context")
                .and_then(|v| v.as_str())
                .filter(|v| !v.starts_with('+'))
                .map(str::to_string),
        ),
        _ => {}
    }
    for (_, value) in uri.params().iter() {
        let Some(value) = value.as_str() else { continue };
        names::node_names(value, &mut out.names);
    }
    // Escaped headers (RFC 3261 §19.1.1) are header values once decoded.
    for (name, value) in uri.escaped_headers() {
        let Some(value) = value else { continue };
        let decoded = sip_message::param_codec::decode_param(value);
        if name.eq_ignore_ascii_case("User-to-User") {
            out.uuis.extend(uui_of(&decoded));
            continue;
        }
        if name.eq_ignore_ascii_case("Replaces") {
            names::call_id_host(&decoded, &mut out.names);
        }
        scan_uris(&SipStr::owned(decoded.as_str()), out);
    }
}

/// Harvest the user part of a URI, its escapes decoded (RFC 3261 §19.1.2),
/// the user proper read apart from its user parameters (`;npdi;rn=…`, RFC
/// 4694): a user proper written as a dotted quad is an address, the address
/// pass's, and yields no number; any other user opening on a digit run
/// (behind an optional `+`) is a number, its visual separators taken out
/// (RFC 3966 §5.1.1) and its parameters kept behind it, and the written
/// spelling of the user proper is kept when separators or escapes stand in
/// it; any other user with a letter is a name, as written. What a
/// number-led user carries behind its digits (`+15556345678.tag.ac14cc94`)
/// is a name.
fn push_uri_number(out: &mut Harvested, uri: &Uri) {
    use sip_message::param_codec::decode_param;
    let written = match uri.scheme() {
        "sip" | "sips" => uri.user(),
        // A tel URI has no user/host split; the number parses as the host.
        "tel" => Some(uri.host()),
        _ => None,
    };
    let Some(written) = written else { return };
    let (written_proper, written_params) =
        written.split_at(written.find(';').unwrap_or(written.len()));
    let (proper, params) = (decode_param(written_proper), decode_param(written_params));
    // A user parameter's dotted node name (`;tg=GROUP.SITE`) names a network.
    let param_names = |out: &mut Harvested| {
        for value in params.split(';').filter_map(|p| p.split_once('=').map(|(_, v)| v)) {
            names::node_names(value, &mut out.names);
        }
    };
    if is_address(written_proper) {
        param_names(out);
        return;
    }
    let digits = proper.strip_prefix('+').unwrap_or(&proper);
    if digits.starts_with(|c: char| c.is_ascii_digit()) {
        let phone = digits.bytes().all(|b| b.is_ascii_digit() || b"-.()".contains(&b));
        let separated = phone && digits.bytes().any(|b| b"-.()".contains(&b));
        if separated || written_proper.contains('%') {
            out.separated.push(written_proper.to_string());
        }
        let digits_end =
            proper.len() - digits.len() + digits.bytes().take_while(u8::is_ascii_digit).count();
        let rest = &proper[digits_end..];
        if !separated && rest.bytes().any(|b| b.is_ascii_alphabetic()) {
            out.names.texts.push(rest.to_string());
        }
        param_names(out);
        let number: String = match separated {
            true => proper.chars().filter(|c| !"-.()".contains(*c)).collect(),
            false => proper.clone(),
        };
        out.numbers.push(number + &params);
    } else if proper.bytes().chain(params.bytes()).any(|b| b.is_ascii_alphabetic()) {
        out.names.texts.push(written.to_string());
    }
}

/// Whether a user proper, as written, is a dotted quad of canonical octets
/// (no leading zero but a bare `0`): an address, where `010.123.45.67` is a
/// trunk-prefixed national number.
fn is_address(written: &str) -> bool {
    ipv4_octets(written).is_some()
        && written.split('.').all(|octet| octet == "0" || !octet.starts_with('0'))
}
