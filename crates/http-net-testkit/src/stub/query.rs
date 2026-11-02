//! A request target read as a path and a query (RFC 3986 §3.4), the query's
//! parameters decoded as `application/x-www-form-urlencoded` pairs.

/// `target` split at its first `?`: the path, and the query where one is there.
pub(super) fn split(target: &str) -> (&str, Option<&str>) {
    match target.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (target, None),
    }
}

/// The query's parameters in order, each name and value decoded: `+` is a
/// space and `%XX` a byte, a byte sequence that is not UTF-8 replaced
/// lossily. An empty segment (`a=1&&b=2`) is no parameter; a segment with no
/// `=` is a name with an empty value.
pub(super) fn params(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|segment| !segment.is_empty())
        .map(|segment| {
            let (name, value) = segment.split_once('=').unwrap_or((segment, ""));
            (decode(name), decode(value))
        })
        .collect()
}

/// One form-encoded component decoded; a `%` not followed by two hex digits
/// stays as it is.
fn decode(component: &str) -> String {
    let bytes = component.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&byte) = bytes.get(i) {
        match byte {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' => match (bytes.get(i + 1).and_then(hex), bytes.get(i + 2).and_then(hex)) {
                (Some(high), Some(low)) => {
                    out.push(high << 4 | low);
                    i += 3;
                }
                _ => {
                    out.push(b'%');
                    i += 1;
                }
            },
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(digit: &u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_target_splits_at_its_first_question_mark() {
        assert_eq!(split("/a/b?x=1?y"), ("/a/b", Some("x=1?y")));
        assert_eq!(split("/a"), ("/a", None));
        assert_eq!(split("/a?"), ("/a", Some("")));
    }

    #[test]
    fn parameters_decode_plus_and_percent_escapes() {
        let got = params("tn=%2B33+1&id=a%20b&flag&&bad=%zz%4");
        let want = [("tn", "+33 1"), ("id", "a b"), ("flag", ""), ("bad", "%zz%4")];
        assert_eq!(got, want.map(|(n, v)| (n.to_string(), v.to_string())));
    }

    #[test]
    fn an_escape_that_is_not_utf8_decodes_lossily() {
        assert_eq!(params("v=%ff"), vec![("v".to_string(), "\u{fffd}".to_string())]);
    }
}
