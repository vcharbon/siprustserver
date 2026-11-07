//! MSCML (RFC 5022) request/response correlation, read and rewritten at the
//! string level.
//!
//! An MSCML body is one `<MediaServerControl>` root holding one `<request>`,
//! `<response>` or `<notification>`. A request's command element (`<play>`,
//! `<playcollect>`, `<stop>` …) MAY carry a client-defined `id` (§4.1), and the
//! `<response>` that answers it echoes that `id` beside its `request` attribute,
//! which names the command type (§10.2) and reports its outcome in `code`
//! and `reason`. This module reads those attributes and sets or removes a
//! response's `id`, every other byte of the body kept as it was. It is no XML
//! parser: it walks the leading tags only, skipping the declaration, comments
//! and processing instructions, and reads attribute values in either XML
//! quote, raw (entities are not expanded).

use crate::header::{HeaderValue, MediaType};
use crate::multipart::entity_parts;
use crate::sip_str::SipStr;

/// The MSCML media type (RFC 5022 §12.1).
pub const CONTENT_TYPE: &str = "application/mediaservercontrol+xml";

/// The root element every MSCML body holds (RFC 5022 §4.1).
const ROOT: &str = "MediaServerControl";

/// One MSCML request: the command element inside `<request>` and the
/// client-defined id it carries, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// The command element's name — the token a response's `request` names.
    pub command: String,
    /// The command element's `id` attribute, raw.
    pub id: Option<String>,
}

/// What one MSCML `<response>` reports (RFC 5022 §10.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// The `request` attribute: the command type answered, raw.
    pub request: Option<String>,
    /// The `code` attribute where it is a status code of three digits (an
    /// RFC 3261 status class, §10.2); `None` where it is absent or anything
    /// else.
    pub code: Option<u16>,
    /// The `reason` attribute, raw.
    pub reason: Option<String>,
}

/// Whether `content_type` names the MSCML media type, parameters and casing
/// aside.
pub fn is_mscml(content_type: &str) -> bool {
    MediaType::parse(&SipStr::owned(content_type)).is_ok_and(|media| media.is(CONTENT_TYPE))
}

/// The request an MSCML body holds, or `None` where its root holds no
/// `<request>` with a command element.
pub fn request(xml: &str) -> Option<Request> {
    let mut tags = Tags::new(xml);
    let wrapper = body_element(&mut tags)?;
    if wrapper.name != "request" || wrapper.empty {
        return None;
    }
    let command = tags.next().filter(|tag| !tag.close)?;
    Some(Request {
        command: command.name.to_string(),
        id: attribute(xml, &command, "id").map(|a| a.value.to_string()),
    })
}

/// Every MSCML request a message body carries under `content_type`: the whole
/// body where it is MSCML, each MSCML part of a `multipart/…` body.
pub fn requests_in(content_type: Option<&str>, body: &[u8]) -> Vec<Request> {
    entity_parts(content_type, &[], body)
        .iter()
        .filter(|part| is_mscml(&part.content_type))
        .filter_map(|part| std::str::from_utf8(&part.payload).ok().and_then(request))
        .collect()
}

/// The response an MSCML body holds, or `None` where its root holds no
/// `<response>`. Only the attributes of the response element itself are
/// read: a name that merely ends in `code`, or `code=` inside another
/// attribute's value, is not its code.
pub fn response(xml: &str) -> Option<Response> {
    let element = response_element(xml)?;
    let value = |name: &str| attribute(xml, &element, name).map(|a| a.value);
    Some(Response {
        request: value("request").map(str::to_string),
        code: value("code").and_then(three_digits),
        reason: value("reason").map(str::to_string),
    })
}

/// Every MSCML response a message body carries under `content_type`, read by
/// the rule of [`requests_in`].
pub fn responses_in(content_type: Option<&str>, body: &[u8]) -> Vec<Response> {
    entity_parts(content_type, &[], body)
        .iter()
        .filter(|part| is_mscml(&part.content_type))
        .filter_map(|part| std::str::from_utf8(&part.payload).ok().and_then(response))
        .collect()
}

/// `value` as a three-digit code, nothing else around it.
fn three_digits(value: &str) -> Option<u16> {
    if value.len() != 3 || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

/// The `request` attribute of an MSCML body's `<response>` — the command type
/// it answers — or `None` where the root holds no response stating one.
pub fn response_request(xml: &str) -> Option<&str> {
    let response = response_element(xml)?;
    attribute(xml, &response, "request").map(|a| a.value)
}

/// `xml` with its `<response>` element's `id` set to `id`, inserted after the
/// element name where it states none, or removed where `id` is `None`. A body
/// holding no response comes back unchanged.
pub fn with_response_id(xml: &str, id: Option<&str>) -> String {
    response_id_set(xml, id).unwrap_or_else(|| xml.to_string())
}

/// [`with_response_id`], `None` where the body comes back unchanged.
fn response_id_set(xml: &str, id: Option<&str>) -> Option<String> {
    let response = response_element(xml)?;
    let stated = attribute(xml, &response, "id");
    let mut out = String::with_capacity(xml.len() + id.map_or(0, |v| v.len() + 6));
    match (stated, id) {
        (None, None) => return None,
        (None, Some(value)) => {
            let quote = quote_for(value, '"');
            out.push_str(xml.get(..response.name_end)?);
            out.push_str(&format!(" id={quote}{value}{quote}"));
            out.push_str(xml.get(response.name_end..)?);
        }
        (Some(attr), Some(value)) => {
            let quote = quote_for(value, attr.quote);
            out.push_str(xml.get(..attr.value_start.checked_sub(1)?)?);
            out.push_str(&format!("{quote}{value}{quote}"));
            out.push_str(xml.get(attr.value_end + 1..)?);
        }
        (Some(attr), None) => {
            out.push_str(xml.get(..attr.start)?);
            out.push_str(xml.get(attr.value_end + 1..)?);
        }
    }
    Some(out)
}

/// The quote a value rides in: `preferred`, unless the value holds it.
fn quote_for(value: &str, preferred: char) -> char {
    match (value.contains(preferred), preferred) {
        (false, _) => preferred,
        (true, '"') => '\'',
        (true, _) => '"',
    }
}

/// The root's `<response>` element, where the body holds one.
fn response_element(xml: &str) -> Option<Tag<'_>> {
    body_element(&mut Tags::new(xml)).filter(|tag| tag.name == "response")
}

/// The element the `<MediaServerControl>` root holds — its first child.
fn body_element<'a>(tags: &mut Tags<'a>) -> Option<Tag<'a>> {
    tags.next().filter(|tag| tag.name == ROOT && !tag.close && !tag.empty)?;
    tags.next().filter(|tag| !tag.close)
}

/// One element tag: its name and where it sits in the body.
#[derive(Debug, Clone, Copy)]
struct Tag<'a> {
    name: &'a str,
    /// Where the name ends: the attributes start here.
    name_end: usize,
    /// Where the tag's `>` (or `/>`) starts.
    attrs_end: usize,
    /// `</name>`.
    close: bool,
    /// `<name … />`.
    empty: bool,
}

/// The element tags of a body in document order; the XML declaration,
/// processing instructions, comments, CDATA sections and a DOCTYPE are
/// skipped, and so is character data.
struct Tags<'a> {
    xml: &'a str,
    at: usize,
}

impl<'a> Tags<'a> {
    fn new(xml: &'a str) -> Self {
        Tags { xml, at: 0 }
    }
}

impl<'a> Iterator for Tags<'a> {
    type Item = Tag<'a>;

    fn next(&mut self) -> Option<Tag<'a>> {
        let xml = self.xml;
        loop {
            let open = self.at + xml.get(self.at..)?.find('<')?;
            let rest = xml.get(open..)?;
            let skip = [("<?", "?>"), ("<!--", "-->"), ("<![CDATA[", "]]>"), ("<!", ">")]
                .into_iter()
                .find_map(|(lead, end)| Some((lead, end, rest.strip_prefix(lead)?)));
            if let Some((lead, end, inner)) = skip {
                self.at = open + lead.len() + inner.find(end)? + end.len();
                continue;
            }
            let close = rest.starts_with("</");
            let name_start = open + if close { 2 } else { 1 };
            let name_end = name_start
                + xml
                    .get(name_start..)?
                    .find(|c: char| c.is_ascii_whitespace() || c == '/' || c == '>')?;
            // A `>` inside a quoted attribute value does not end the tag.
            let mut quote = None;
            let mut gt = None;
            for (offset, c) in xml.get(name_end..)?.char_indices() {
                match (quote, c) {
                    (Some(q), c) if c == q => quote = None,
                    (Some(_), _) => {}
                    (None, '"' | '\'') => quote = Some(c),
                    (None, '>') => {
                        gt = Some(name_end + offset);
                        break;
                    }
                    (None, _) => {}
                }
            }
            let gt = gt?;
            let empty = !close && xml.get(..gt)?.ends_with('/');
            self.at = gt + 1;
            return Some(Tag {
                name: xml.get(name_start..name_end)?,
                name_end,
                attrs_end: if empty { gt.checked_sub(1)? } else { gt },
                close,
                empty,
            });
        }
    }
}

/// One attribute of a tag, located in the body.
#[derive(Debug, Clone, Copy)]
struct Attribute<'a> {
    /// Where the whitespace that leads the attribute starts.
    start: usize,
    /// The raw value, between its quotes.
    value: &'a str,
    value_start: usize,
    value_end: usize,
    quote: char,
}

/// The attribute `name` of `tag`, where it states one in well-formed shape.
fn attribute<'a>(xml: &'a str, tag: &Tag<'_>, name: &str) -> Option<Attribute<'a>> {
    let end = tag.attrs_end;
    let within = |from: usize| xml.get(from..end);
    let lead_ws = |from: usize| within(from).map(|s| s.len() - s.trim_start().len());
    let mut at = tag.name_end;
    loop {
        let start = at;
        at += lead_ws(at)?;
        if at >= end {
            return None;
        }
        let name_end = at + within(at)?.find(|c: char| c.is_ascii_whitespace() || c == '=')?;
        let attr_name = xml.get(at..name_end)?;
        at = name_end;
        at += lead_ws(at)?;
        if !within(at)?.starts_with('=') {
            return None;
        }
        at += 1;
        at += lead_ws(at)?;
        let quote = within(at)?.chars().next().filter(|c| *c == '"' || *c == '\'')?;
        let value_start = at + 1;
        let value_end = value_start + within(value_start)?.find(quote)?;
        at = value_end + 1;
        if attr_name == name {
            return Some(Attribute {
                start,
                value: xml.get(value_start..value_end)?,
                value_start,
                value_end,
                quote,
            });
        }
    }
}

#[cfg(test)]
mod tests;
