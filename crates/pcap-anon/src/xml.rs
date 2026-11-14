//! The data of an XML control document: every attribute value and every text
//! node, each with the delimiters it is written between (`"…"`, `'…'`,
//! `>…<`) — what the anonymiser rewrites in place and the leak check reads.
//! Element and attribute names are the schema's and never data; a
//! declaration, a comment or a CDATA section is skipped. The value of a
//! control-protocol attribute is protocol text ([`Datum::is_protocol`]). A
//! part is read as a control document only when [`is_document`] holds.

/// The attributes of the media-server control protocol (MSCML, RFC 5022)
/// whose values are protocol — an enumeration, a version, a timer, a count,
/// a response's text — never data a party supplied.
const PROTOCOL_ATTRIBUTES: &[&str] = &[
    "version",
    "request",
    "code",
    "reason",
    "text",
    "barge",
    "cleardigits",
    "repeat",
    "maxdigits",
    "firstdigittimer",
    "interdigittimer",
    "extradigittimer",
    "interdigitcriticaltimer",
    "returnkey",
    "escapekey",
    "stoponerror",
    "encoding",
    "playduration",
    "playoffset",
    "duration",
    "offset",
    "gain",
    "gaindelta",
    "rate",
    "ratedelta",
];

/// One piece of data, as written: the delimiter before it, the value, the
/// delimiter after it, the attribute it is the value of (lowercase; `None`
/// for a text node) and the byte offset of the value in the document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Datum {
    pub open: char,
    pub value: String,
    pub close: char,
    pub attribute: Option<String>,
    pub at: usize,
}

impl Datum {
    /// The datum with its delimiters, as it stands in the document.
    pub fn delimited(&self) -> String {
        format!("{}{}{}", self.open, self.value, self.close)
    }

    /// Whether the datum is protocol text: the value of a control-protocol
    /// attribute ([`PROTOCOL_ATTRIBUTES`]), in which only the hosts and
    /// numbers are identities.
    pub fn is_protocol(&self) -> bool {
        self.attribute.as_deref().is_some_and(|a| PROTOCOL_ATTRIBUTES.contains(&a))
    }
}

/// Every attribute value and non-blank text node of `text`, in document
/// order.
pub fn data(text: &str) -> Vec<Datum> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            let end = bytes[i..].iter().position(|b| *b == b'<').map_or(bytes.len(), |p| i + p);
            if i > 0 && bytes[i - 1] == b'>' && end < bytes.len() && !text[i..end].trim().is_empty()
            {
                let value = text[i..end].to_string();
                out.push(Datum { open: '>', value, close: '<', attribute: None, at: i });
            }
            i = end;
            continue;
        }
        let close = bytes[i..].iter().position(|b| *b == b'>').map_or(bytes.len(), |p| i + p);
        if !matches!(bytes.get(i + 1), Some(b'?' | b'!' | b'/')) {
            attributes(&text[i..close], i, &mut out);
        }
        i = close + 1;
    }
    out
}

/// Whether every byte of `text` is either markup [`data`] reads past or data
/// it returns: the text, trimmed, opens on `<` and closes on `>`; each tag is
/// an element's name and its `name="value"` attributes (an end tag, its name
/// alone), or the XML declaration. A comment, a CDATA section, a doctype or a
/// processing instruction — text [`data`] never reads — makes it no
/// document.
pub fn is_document(text: &str) -> bool {
    let text = text.trim_matches(|c: char| c.is_ascii_whitespace());
    if !(text.starts_with('<') && text.ends_with('>')) {
        return false;
    }
    let mut rest = text;
    while let Some(open) = rest.find('<') {
        let Some(close) = rest[open..].find('>').map(|p| open + p) else { return false };
        if !tag_reads(&rest[open + 1..close]) {
            return false;
        }
        rest = &rest[close + 1..];
    }
    true
}

/// Whether the inside of one tag (between `<` and `>`) is an element's start,
/// empty or end tag, or the XML declaration.
fn tag_reads(tag: &str) -> bool {
    if let Some(declaration) = tag.strip_prefix("?xml").and_then(|t| t.strip_suffix('?')) {
        return declaration.starts_with(|c: char| c.is_ascii_whitespace())
            && attributes_read(declaration);
    }
    if let Some(end) = tag.strip_prefix('/') {
        return is_name(end.trim_end());
    }
    let tag = tag.strip_suffix('/').unwrap_or(tag);
    let name_end = tag.find(|c: char| c.is_ascii_whitespace()).unwrap_or(tag.len());
    is_name(&tag[..name_end]) && attributes_read(&tag[name_end..])
}

/// Whether `text` is nothing but whitespace-separated `name="value"` (or
/// `'value'`) attributes.
fn attributes_read(mut text: &str) -> bool {
    loop {
        text = text.trim_start_matches(|c: char| c.is_ascii_whitespace());
        if text.is_empty() {
            return true;
        }
        let Some((name, value)) = text.split_once('=') else { return false };
        let value = value.trim_start_matches(|c: char| c.is_ascii_whitespace());
        let Some(quote) = value.chars().next().filter(|c| matches!(c, '"' | '\'')) else {
            return false;
        };
        let Some(end) = value[1..].find(quote) else { return false };
        if !is_name(name.trim_end()) {
            return false;
        }
        text = &value[end + 2..];
        if !text.is_empty() && !text.starts_with(|c: char| c.is_ascii_whitespace()) {
            return false;
        }
    }
}

/// Whether `name` is an XML name: a letter, `_` or `:`, then letters, digits,
/// `-`, `.`, `_` or `:`.
fn is_name(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_alphabetic() || matches!(c, '_' | ':'))
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | ':'))
}

/// The attribute values of one tag, which opens at `base` in the document.
fn attributes(tag: &str, base: usize, out: &mut Vec<Datum>) {
    let bytes = tag.as_bytes();
    let mut i = 0;
    while let Some(eq) = bytes[i..].iter().position(|b| *b == b'=').map(|p| i + p) {
        let mut q = eq + 1;
        while bytes.get(q).is_some_and(u8::is_ascii_whitespace) {
            q += 1;
        }
        let Some(&quote) = bytes.get(q).filter(|b| matches!(b, b'"' | b'\'')) else {
            i = eq + 1;
            continue;
        };
        let Some(end) = bytes[q + 1..].iter().position(|b| *b == quote).map(|p| q + 1 + p) else {
            return;
        };
        let quote = char::from(quote);
        let name_end = tag[..eq].trim_end().len();
        let name_start =
            tag[..name_end].rfind(|c: char| c.is_ascii_whitespace()).map_or(0, |p| p + 1);
        let attribute = Some(tag[name_start..name_end].to_ascii_lowercase());
        let value = tag[q + 1..end].to_string();
        out.push(Datum { open: quote, value, close: quote, attribute, at: base + q + 1 });
        i = end + 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attribute_values_and_text_nodes_are_the_data() {
        let doc =
            "<?xml version=\"1.0\"?><a x=\"1\" y='two'><b>Fakey Testo</b>\r\n<c/><!-- n --></a>";
        let found: Vec<String> = data(doc).iter().map(Datum::delimited).collect();
        assert_eq!(found, ["\"1\"", "'two'", ">Fakey Testo<"]);
        let named: Vec<Option<String>> = data(doc).into_iter().map(|d| d.attribute).collect();
        assert_eq!(named, [Some("x".into()), Some("y".into()), None]);
        for d in data(doc) {
            assert_eq!(&doc[d.at..d.at + d.value.len()], d.value, "where the value stands");
        }
    }

    #[test]
    fn a_document_is_markup_and_data_alone() {
        assert!(is_document(
            "\r\n<?xml version=\"1.0\"?>\r\n<a x=\"1\" y = 'two'><b>Fakey Testo</b><c/></a>\r\n"
        ));
        for no in [
            "dest=5550100;name=Fakey Testo",
            "Fakey <a/>",
            "<a/> Fakey",
            "<a Fakey Testo=\"1\"/>",
            "<a x=1/>",
            "<a x=\"1\"y=\"2\"/>",
            "<a><!-- Fakey --></a>",
            "<a><![CDATA[Fakey]]></a>",
            "<?Fakey Testo?><a/>",
            "</Fakey Testo>",
        ] {
            assert!(!is_document(no), "{no}");
        }
    }

    #[test]
    fn a_control_protocol_attribute_is_protocol_and_a_data_attribute_or_text_is_not() {
        let doc = "<response request=\"play\" code=\"200\" reason=\"EOF\" text=\"OK\"/>\
                   <var name=\"caller\" value=\"Fakey\"/><who>Fakey</who>";
        let protocol: Vec<bool> = data(doc).iter().map(Datum::is_protocol).collect();
        assert_eq!(protocol, [true, true, true, true, false, false, false]);
    }
}
