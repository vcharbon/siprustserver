//! The free-form fields of a session description (RFC 4566 §5.2–§5.6): the
//! origin's username, the session name, and every information, URI, email
//! and phone field at session or media level — the text a description carries
//! for people rather than for the media stack.

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

#[cfg(test)]
mod tests {
    use super::*;

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
