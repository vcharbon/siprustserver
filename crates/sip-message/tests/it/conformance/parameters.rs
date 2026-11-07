//! Whether a header line names one parameter twice in one scope: RFC 3261
//! §7.3.1 for a header value's parameters, §19.1.1 for a URI's. A scope is one
//! comma-separated value, or one URI inside `<...>`. A URI's escaped headers
//! (`?...`) are no parameters.

/// How a header lays out its parameters.
#[derive(Clone, Copy, Default)]
pub struct Layout {
    /// The credentials and challenge layout: `scheme LWS name=value` then
    /// `, name=value`, the whole line one scope.
    pub comma_separated: bool,
    /// The grammar has comments, so a `(...)` holds no parameter.
    pub comments: bool,
}

/// The parameter names of one scope.
#[derive(Default)]
struct Scope(Vec<String>);

impl Scope {
    /// Notes `name`, `true` when the scope already holds it.
    fn repeats(&mut self, name: &[u8]) -> bool {
        let name = String::from_utf8_lossy(name).trim().to_ascii_lowercase();
        if name.is_empty() {
            return false;
        }
        if self.0.contains(&name) {
            return true;
        }
        self.0.push(name);
        false
    }
}

/// The name that starts at `at`, up to its value or the next separator.
fn name_at(line: &[u8], at: usize) -> &[u8] {
    let rest = &line[at..];
    let end = rest.iter().position(|b| b"=;,>?\"\r\n".contains(b)).unwrap_or(rest.len());
    &rest[..end]
}

/// Whether `line` (`name: value`, folds and all) repeats a parameter name.
pub fn repeats_a_name(line: &[u8], layout: Layout) -> bool {
    let start = line.iter().position(|&b| b == b':').map_or(0, |colon| colon + 1);
    let mut header = Scope::default();
    let mut uri = Scope::default();
    let (mut quoted, mut escaped, mut angle, mut comment) = (false, false, false, 0usize);
    let mut uri_headers = false;
    let mut scheme_ended = !layout.comma_separated;
    let mut at = start;
    while at < line.len() {
        let byte = line[at];
        at += 1;
        if quoted {
            match (escaped, byte) {
                (true, _) => escaped = false,
                (false, b'\\') => escaped = true,
                (false, b'"') => quoted = false,
                _ => {}
            }
            continue;
        }
        if comment > 0 {
            match byte {
                b'(' => comment += 1,
                b')' => comment -= 1,
                _ => {}
            }
            continue;
        }
        let scope = if angle { &mut uri } else { &mut header };
        match byte {
            b'"' => quoted = true,
            b'(' if layout.comments && !angle => comment = 1,
            b'<' if !angle => {
                angle = true;
                uri_headers = false;
                uri = Scope::default();
            }
            b'>' if angle => angle = false,
            b'?' if angle => uri_headers = true,
            _ if angle && uri_headers => {}
            b';' => {
                if scope.repeats(name_at(line, at)) {
                    return true;
                }
            }
            b',' if !angle && layout.comma_separated => {
                if header.repeats(name_at(line, at)) {
                    return true;
                }
            }
            b',' if !angle => header = Scope::default(),
            b' ' | b'\t' if !scheme_ended && at - 1 > start && !angle => {
                let before = &line[start..at - 1];
                if before.iter().any(|b| !b" \t\r\n".contains(b)) {
                    scheme_ended = true;
                    if header.repeats(name_at(line, at)) {
                        return true;
                    }
                }
            }
            _ => {}
        }
    }
    false
}

#[test]
fn a_repeat_is_found_in_its_scope_only() {
    let plain = Layout::default();
    assert!(repeats_a_name(b"From: <sip:a@b>;tag=1;TAG=2", plain));
    assert!(repeats_a_name(b"From: <sip:a@b;x=1;x=2>;tag=1", plain));
    assert!(!repeats_a_name(b"From: <sip:a@b;tag=1>;tag=1", plain));
    assert!(!repeats_a_name(b"Via: SIP/2.0/UDP a;branch=1, SIP/2.0/UDP b;branch=2", plain));
    assert!(!repeats_a_name(b"From: \"a;x;x\" <sip:a@b>;x=1", plain));
    assert!(!repeats_a_name(b"Refer-To: <sip:a@b?h=1;x;x>;x=1", plain));
    let credentials = Layout { comma_separated: true, comments: false };
    assert!(repeats_a_name(b"Authorization: Digest realm=\"a\", realm=\"b\"", credentials));
    assert!(!repeats_a_name(b"Authorization: Digest realm=\"a\", nonce=\"b\"", credentials));
    let comments = Layout { comma_separated: false, comments: true };
    assert!(!repeats_a_name(b"Retry-After: 120 (a;x;x);duration=1", comments));
}
