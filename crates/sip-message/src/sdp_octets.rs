//! The octets of a session description carried through a rewrite made on
//! text. RFC 4566 §5 makes text fields octet strings, so a byte that is no
//! UTF-8 still belongs to the description: [`decode`] stands each such byte
//! for a private-use character (U+10FF00 plus the byte) and [`encode`] writes
//! it back, so a rewrite keeps every byte it does not change. A description
//! already holding one of those characters has no such reading.

/// The first stand-in; the byte is its low eight bits.
const STAND_IN_BASE: u32 = 0x10_FF00;

/// The byte `c` stands for, where it is a stand-in.
fn stood_for(c: char) -> Option<u8> {
    let code = u32::from(c);
    (code & !0xFF == STAND_IN_BASE).then(|| u8::try_from(code & 0xFF).ok()).flatten()
}

/// `bytes` as text, each byte that is no UTF-8 as its stand-in; `None` where
/// the text already holds a stand-in.
pub(crate) fn decode(bytes: &[u8]) -> Option<String> {
    let mut text = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        let valid = chunk.valid();
        if valid.chars().any(|c| stood_for(c).is_some()) {
            return None;
        }
        text.push_str(valid);
        for &byte in chunk.invalid() {
            text.push(char::from_u32(STAND_IN_BASE | u32::from(byte))?);
        }
    }
    Some(text)
}

/// `text` as octets, each stand-in as the byte it stands for.
pub(crate) fn encode(text: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len());
    let mut buf = [0u8; 4];
    for c in text.chars() {
        match stood_for(c) {
            Some(byte) => bytes.push(byte),
            None => bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes()),
        }
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_byte_comes_back() {
        let latin1 = b"v=0\r\ns=Caf\xe9 \xff\xc3\r\nm=audio 0 RTP/AVP 0\r\n";
        let text = decode(latin1).expect("a reading");
        assert!(text.starts_with("v=0\r\ns=Caf"));
        assert_eq!(encode(&text), latin1);
        let utf8 = "v=0\r\ns=Caf\u{e9}\r\n".as_bytes();
        assert_eq!(decode(utf8).as_deref(), Some("v=0\r\ns=Caf\u{e9}\r\n"));
        assert_eq!(encode(&decode(utf8).unwrap()), utf8);
    }

    #[test]
    fn a_text_holding_a_stand_in_has_no_reading() {
        assert_eq!(decode("s=\u{10FF41}".as_bytes()), None);
    }
}
