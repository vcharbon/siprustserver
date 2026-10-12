//! The free-form fields of a session description (RFC 4566 §5.2–§5.6): the
//! origin's username, the session name, and every information, URI, email
//! and phone field at session or media level — the text a description carries
//! for people rather than for the media stack — and its attributes (§5.13).

/// One free-form field: its letter (`o` for the origin username, `s`, `i`,
/// `u`, `e`, `p`) and its value as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextField {
    pub key: char,
    pub value: String,
}

/// Every free-form field of `body`, in document order; nothing where the body
/// is no description (it does not open on `v=`).
pub fn text_fields(body: &[u8]) -> Vec<TextField> {
    let text = String::from_utf8_lossy(body);
    if !text.starts_with("v=") {
        return Vec::new();
    }
    let mut out = Vec::new();
    for line in text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)) {
        let mut chars = line.chars();
        let (Some(key), Some('=')) = (chars.next(), chars.next()) else { continue };
        let value = &line[2..];
        let value = match key {
            'o' => value.split(' ').next().unwrap_or_default(),
            's' | 'i' | 'u' | 'e' | 'p' => value,
            _ => continue,
        };
        out.push(TextField { key, value: value.to_string() });
    }
    out
}

/// One attribute line (`a=<name>` or `a=<name>:<value>`, RFC 4566 §5.13).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribute {
    pub name: String,
    pub value: Option<String>,
}

/// Every attribute of `body`, session and media level, in document order;
/// nothing where the body is no description (it does not open on `v=`).
pub fn attributes(body: &[u8]) -> Vec<Attribute> {
    let text = String::from_utf8_lossy(body);
    if !text.starts_with("v=") {
        return Vec::new();
    }
    text.split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .filter_map(|line| line.strip_prefix("a="))
        .map(|attr| match attr.split_once(':') {
            Some((name, value)) => {
                Attribute { name: name.to_string(), value: Some(value.to_string()) }
            }
            None => Attribute { name: attr.to_string(), value: None },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_attribute_is_read_with_its_value_where_it_has_one() {
        let body = b"v=0\r\no=- 1 1 IN IP4 192.0.2.1\r\ns=-\r\nt=0 0\r\na=sendrecv\r\n\
                     m=audio 4000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=x-ref:http://a.example/x\r\n";
        let attrs = attributes(body);
        assert_eq!(
            attrs.iter().map(|a| (a.name.as_str(), a.value.as_deref())).collect::<Vec<_>>(),
            [
                ("sendrecv", None),
                ("rtpmap", Some("0 PCMU/8000")),
                ("x-ref", Some("http://a.example/x"))
            ]
        );
        assert!(attributes(b"a=x:y").is_empty());
    }

    #[test]
    fn every_free_form_field_is_read_at_session_and_media_level() {
        let body = b"v=0\r\no=alice 1 1 IN IP4 192.0.2.1\r\ns=A call\r\ni=About it\r\n\
                     u=http://www.example.com/x\r\ne=alice@example.com\r\np=+1 555 0100\r\n\
                     c=IN IP4 192.0.2.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 0\r\ni=The voice\r\n";
        let fields = text_fields(body);
        assert_eq!(
            fields.iter().map(|f| (f.key, f.value.as_str())).collect::<Vec<_>>(),
            [
                ('o', "alice"),
                ('s', "A call"),
                ('i', "About it"),
                ('u', "http://www.example.com/x"),
                ('e', "alice@example.com"),
                ('p', "+1 555 0100"),
                ('i', "The voice"),
            ]
        );
        assert!(text_fields(b"not sdp").is_empty());
    }
}
