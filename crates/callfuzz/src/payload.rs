//! Hostile but RFC 3261-legal payloads.
//!
//! Header values stay inside `TEXT-UTF8char / UTF8-CONT / LWS` (§25.1): any
//! non-control UTF-8, no CR, LF or NUL. Bodies are bytes and may hold anything,
//! invalid UTF-8 included, since Content-Length frames them.

use crate::Rng;

/// One hostile header: (name, value, label).
pub fn header(rng: &mut Rng) -> (String, String, &'static str) {
    let pick = rng.below(16);
    let (name, value, label): (&str, String, &str) = match pick {
        0 => ("X-Fuzz-Emoji", "😀🎉 👨‍👩‍👧‍👦 🇫🇷 🏳️‍🌈".into(), "emoji"),
        1 => ("Subject", "Zoë Ñandú — 東京 مرحبا שלום".into(), "subject-utf8"),
        2 => ("X-Fuzz-Rtl", "\u{202E}gnp.exe\u{202C} \u{200F}rtl".into(), "bidi"),
        3 => ("X-Fuzz-Zw", "a\u{200B}b\u{200D}c\u{FEFF}d\u{2060}".into(), "zero-width"),
        4 => ("X-Fuzz-Long", "é".repeat(100 + rng.below(350) as usize), "long-utf8"),
        5 => (
            "X-Fuzz-Combining",
            "Z\u{334}\u{321}\u{327}\u{31b}\u{31c}\u{31e}a\u{337}l\u{338}g\u{335}o\u{334}".into(),
            "combining",
        ),
        6 => ("X-Fuzz-Astral", "𝕳𝖊𝖑𝖑𝖔 𐍈 🜁 \u{10FFFD}".into(), "astral"),
        7 => (
            "X-Fuzz-Quote",
            "\"quoted ; , \\\" \" <sip:not@a.uri> ;p=\"x\" , comma".into(),
            "separators",
        ),
        8 => ("User-Agent", "fuzz/1.0 (Zoë ☎ (nested (deep)) 😀)".into(), "ua-comment"),
        9 => ("X-Fuzz-NonChar", "\u{FFFE}\u{FFFF}\u{FFFD}\u{1FFFE}".into(), "nonchar"),
        10 => ("x-FUZZ-cAsE", "MiXeD".into(), "name-case"),
        11 => ("s", "compact subject 😀".into(), "compact-subject"),
        12 => ("X-Fuzz-Lws", "a    b\t\tc   ".into(), "lws"),
        13 => ("X-Fuzz-Huge", "ü".repeat(600 + rng.below(1200) as usize), "huge"),
        14 => ("X-Fuzz-Percent", "%00%0D%0A%FF%C3%28 %%%".into(), "percent"),
        _ => ("X-Fuzz-Ascii-Edge", "!#$%&'*+-.^_`|~ {}[]\\/?=@:".into(), "ascii-edge"),
    };
    (name.to_string(), value, label)
}

/// The label of the one body an RFC refuses, drawn only when `illegal`.
pub const ILLEGAL_BODY: &str = "ILLEGAL-multipart-utf8-boundary";

/// One odd body: (content type, bytes, label). `illegal` lets the draw yield
/// [`ILLEGAL_BODY`]; otherwise its draw yields the UTF-8 text body.
pub fn body(rng: &mut Rng, illegal: bool) -> (String, Vec<u8>, &'static str) {
    let mut pick = rng.below(9);
    if pick == 5 && !illegal {
        pick = 0;
    }
    match pick {
        0 => ("text/plain;charset=utf-8".into(), "héllo 😀 東京\r\n".as_bytes().to_vec(), "utf8"),
        1 => {
            let n = rng.below(1200) as usize;
            let bytes = (0..n).map(|_| rng.below(256) as u8).collect();
            ("application/octet-stream".into(), bytes, "random-bytes")
        }
        2 => (
            "text/plain;charset=utf-8".into(),
            vec![b'a', 0xC3, 0x28, 0xFF, 0xC0, 0xAF, 0xED, 0xA0, 0x80, 0xF4, 0x90, b'z'],
            "bad-utf8",
        ),
        3 => (
            "application/json".into(),
            "{\"ü\":\"😀\",\"nul\":\"\\u0000\",\"deep\":[[[[[[]]]]]]}".as_bytes().to_vec(),
            "json",
        ),
        4 => (
            "text/plain".into(),
            b"\r\n\r\nINVITE sip:evil@x SIP/2.0\r\nContent-Length: 0\r\n\r\n".to_vec(),
            "embedded-sip",
        ),
        5 => (
            "multipart/mixed;boundary=\"b😀\"".into(),
            "--b😀\r\nContent-Type: text/plain\r\n\r\nzoë\r\n--b😀--\r\n".as_bytes().to_vec(),
            ILLEGAL_BODY,
        ),
        6 => ("application/x-fuzz".into(), Vec::new(), "empty"),
        8 => multipart_binary(rng),
        _ => (
            "text/plain;charset=utf-8".into(),
            vec![0u8, 0, b'\r', b'\n', 0, b'\n', b'\r'],
            "nul-crlf",
        ),
    }
}

/// A two-part multipart body whose second part is drawn binary.
fn multipart_binary(rng: &mut Rng) -> (String, Vec<u8>, &'static str) {
    let mut b = b"--bnd42\r\nContent-Type: text/plain;charset=utf-8\r\n\r\nzo\xc3\xab \xf0\x9f\x98\x80\r\n--bnd42\r\nContent-Type: application/octet-stream\r\nContent-Transfer-Encoding: binary\r\n\r\n".to_vec();
    b.extend((0..rng.below(300)).map(|_| rng.below(256) as u8));
    b.extend_from_slice(b"\r\n--bnd42--\r\n");
    ("multipart/mixed;boundary=bnd42".into(), b, "multipart-binary")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_values_hold_no_control_character() {
        let mut rng = Rng::new(5);
        for _ in 0..2000 {
            let (name, value, _) = header(&mut rng);
            assert!(name.bytes().all(|b| b.is_ascii_graphic()), "{name}");
            assert!(!value.chars().any(|c| matches!(c, '\r' | '\n' | '\0')), "{name}: {value:?}");
        }
    }

    #[test]
    fn the_illegal_body_is_drawn_only_when_asked() {
        let labels = |illegal| {
            let mut rng = Rng::new(9);
            (0..2000).map(|_| body(&mut rng, illegal).2).collect::<Vec<_>>()
        };
        assert!(!labels(false).contains(&ILLEGAL_BODY));
        assert!(labels(true).contains(&ILLEGAL_BODY));
    }
}
