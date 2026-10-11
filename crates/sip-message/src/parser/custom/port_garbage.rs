//! The strict-reader checks of a port written with letters after its digits
//! (`host:5060abc`): in a Request-URI and in a Via sent-by.

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
    if i < s.len() && s[i] == b'[' {
        match s[i + 1..].iter().position(|&c| c == b']') {
            None => return false,
            Some(close) => i = i + 1 + close + 1,
        }
    } else {
        while i < s.len() && s[i] != b':' && s[i] != b';' && s[i] != b'?' && s[i] != b'>' {
            i += 1;
        }
    }
    if i >= s.len() || s[i] != b':' {
        return false;
    }
    i += 1;
    let port_start = i;
    while i < s.len() && s[i].is_ascii_digit() {
        i += 1;
    }
    if i == port_start || i >= s.len() {
        return false;
    }
    s[i].is_ascii_alphabetic()
}

/// True iff the sent-by of `via` carries a port followed by alphabetic
/// trailing garbage (`host:5060abc`). Only the sent-by is read (RFC 3261
/// §20.42: `sent-protocol LWS sent-by`, the sent-protocol three tokens joined
/// by `SLASH = SWS "/" SWS`, `sent-by = host [COLON port]`): an IPv6
/// reference is skipped whole, so its groups are never taken for a port, and
/// the parameters after it (`received`, `maddr`) are not ports.
pub(super) fn has_via_port_trailing_garbage(via: &str) -> bool {
    let s = via.trim_start().as_bytes();
    let skip_ws = |mut i: usize| {
        while i < s.len() && s[i].is_ascii_whitespace() {
            i += 1;
        }
        i
    };
    let token = |mut i: usize| {
        while i < s.len() && !s[i].is_ascii_whitespace() && s[i] != b'/' {
            i += 1;
        }
        i
    };
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
        match s[i..].iter().position(|&b| b == b']') {
            Some(close) => i += close + 1,
            None => return false,
        }
    } else {
        while i < s.len() && !matches!(s[i], b':' | b';' | b',') && !s[i].is_ascii_whitespace() {
            i += 1;
        }
    }
    i = skip_ws(i);
    if s.get(i) != Some(&b':') {
        return false;
    }
    let port_start = skip_ws(i + 1);
    let port_end = port_start + s[port_start..].iter().take_while(|b| b.is_ascii_digit()).count();
    port_end > port_start && s.get(port_end).is_some_and(u8::is_ascii_alphabetic)
}
