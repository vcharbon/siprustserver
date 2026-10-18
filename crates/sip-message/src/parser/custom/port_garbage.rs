//! The strict-reader checks of a port written with letters after its digits
//! (`host:5060abc`): in a Request-URI and in a Via sent-by.

/// The index of the first byte at or after `i` that `stop` holds for, or the
/// end.
fn scan(s: &[u8], i: usize, stop: impl Fn(u8) -> bool) -> usize {
    s.iter().skip(i).position(|&c| stop(c)).map_or(s.len().max(i), |p| i + p)
}

/// True iff `uri` has port digits followed by an alphabetic byte at the
/// host:port position (SIP-ALG confusion), context-aware about userinfo.
pub(super) fn has_uri_port_trailing_garbage(uri: &str) -> bool {
    let s = uri.as_bytes();
    let first = match s.iter().position(|&c| c == b'@') {
        Some(a) => a,
        None => match s.iter().position(|&c| c == b':') {
            Some(c) => c,
            None => return false,
        },
    };
    let mut i = first + 1; // past `@` or scheme colon
    if s.get(i) == Some(&b'[') {
        match s.iter().skip(i + 1).position(|&c| c == b']') {
            None => return false,
            Some(close) => i = i + 1 + close + 1,
        }
    } else {
        i = scan(s, i, |c| matches!(c, b':' | b';' | b'?' | b'>'));
    }
    if s.get(i) != Some(&b':') {
        return false;
    }
    let port_start = i + 1;
    let port_end = scan(s, port_start, |c| !c.is_ascii_digit());
    port_end > port_start && s.get(port_end).is_some_and(u8::is_ascii_alphabetic)
}

/// True iff the sent-by of `via` carries a port followed by alphabetic
/// trailing garbage (`host:5060abc`). Only the sent-by is read (RFC 3261
/// §20.42: `sent-protocol LWS sent-by`, the sent-protocol three tokens joined
/// by `SLASH = SWS "/" SWS`, `sent-by = host [COLON port]`): an IPv6
/// reference is skipped whole, so its groups are never taken for a port, and
/// the parameters after it (`received`, `maddr`) are not ports.
pub(super) fn has_via_port_trailing_garbage(via: &str) -> bool {
    let s = via.trim_start().as_bytes();
    let skip_ws = |i: usize| scan(s, i, |c| !c.is_ascii_whitespace());
    let token = |i: usize| scan(s, i, |c| c.is_ascii_whitespace() || c == b'/');
    let mut i = token(0);
    for _ in 0..2 {
        i = skip_ws(i);
        if s.get(i) != Some(&b'/') {
            return false;
        }
        i = token(skip_ws(i + 1));
    }
    i = skip_ws(i);
    if s.get(i) == Some(&b'[') {
        match s.iter().skip(i).position(|&b| b == b']') {
            Some(close) => i += close + 1,
            None => return false,
        }
    } else {
        i = scan(s, i, |c| matches!(c, b':' | b';' | b',') || c.is_ascii_whitespace());
    }
    i = skip_ws(i);
    if s.get(i) != Some(&b':') {
        return false;
    }
    let port_start = skip_ws(i + 1);
    let port_end = scan(s, port_start, |c| !c.is_ascii_digit());
    port_end > port_start && s.get(port_end).is_some_and(u8::is_ascii_alphabetic)
}
