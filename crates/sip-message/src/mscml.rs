//! MSCML (RFC 5022) request/response correlation, read and rewritten at the
//! string level.
//!
//! An MSCML body is one `<MediaServerControl>` root holding one `<request>`,
//! `<response>` or `<notification>`. A request's command element (`<play>`,
//! `<playcollect>`, `<stop>` …) MAY carry a client-defined `id` (§4.1), and the
//! `<response>` that answers it echoes that `id` beside its `request` attribute,
//! which names the command type (§10.2). This module reads those three
//! attributes and sets or removes a response's `id`, every other byte of the
//! body kept as it was. It is no XML parser: it walks the leading tags only,
//! skipping the declaration, comments and processing instructions, and reads
//! attribute values in either XML quote, raw (entities are not expanded).

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
    let Some(response) = response_element(xml) else { return xml.to_string() };
    let stated = attribute(xml, &response, "id");
    let mut out = String::with_capacity(xml.len() + id.map_or(0, |v| v.len() + 6));
    match (stated, id) {
        (None, None) => return xml.to_string(),
        (None, Some(value)) => {
            let quote = quote_for(value, '"');
            out.push_str(&xml[..response.name_end]);
            out.push_str(&format!(" id={quote}{value}{quote}"));
            out.push_str(&xml[response.name_end..]);
        }
        (Some(attr), Some(value)) => {
            let quote = quote_for(value, attr.quote);
            out.push_str(&xml[..attr.value_start - 1]);
            out.push_str(&format!("{quote}{value}{quote}"));
            out.push_str(&xml[attr.value_end + 1..]);
        }
        (Some(attr), None) => {
            out.push_str(&xml[..attr.start]);
            out.push_str(&xml[attr.value_end + 1..]);
        }
    }
    out
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
            let open = self.at + xml[self.at..].find('<')?;
            let rest = &xml[open..];
            let skip = [("<?", "?>"), ("<!--", "-->"), ("<![CDATA[", "]]>"), ("<!", ">")]
                .into_iter()
                .find(|(lead, _)| rest.starts_with(lead));
            if let Some((lead, end)) = skip {
                self.at = open + lead.len() + rest[lead.len()..].find(end)? + end.len();
                continue;
            }
            let close = rest.starts_with("</");
            let name_start = open + if close { 2 } else { 1 };
            let name_end = name_start
                + xml[name_start..]
                    .find(|c: char| c.is_ascii_whitespace() || c == '/' || c == '>')?;
            // A `>` inside a quoted attribute value does not end the tag.
            let mut quote = None;
            let mut gt = None;
            for (offset, c) in xml[name_end..].char_indices() {
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
            let empty = !close && xml[..gt].ends_with('/');
            self.at = gt + 1;
            return Some(Tag {
                name: &xml[name_start..name_end],
                name_end,
                attrs_end: if empty { gt - 1 } else { gt },
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
    let mut at = tag.name_end;
    loop {
        let start = at;
        at += xml[at..end].len() - xml[at..end].trim_start().len();
        if at >= end {
            return None;
        }
        let name_end = at + xml[at..end].find(|c: char| c.is_ascii_whitespace() || c == '=')?;
        let attr_name = &xml[at..name_end];
        at = name_end;
        at += xml[at..end].len() - xml[at..end].trim_start().len();
        if !xml[at..end].starts_with('=') {
            return None;
        }
        at += 1;
        at += xml[at..end].len() - xml[at..end].trim_start().len();
        let quote = xml[at..end].chars().next().filter(|c| *c == '"' || *c == '\'')?;
        let value_start = at + 1;
        let value_end = value_start + xml[value_start..end].find(quote)?;
        at = value_end + 1;
        if attr_name == name {
            return Some(Attribute {
                start,
                value: &xml[value_start..value_end],
                value_start,
                value_end,
                quote,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DECL: &str = r#"<?xml version="1.0" encoding="utf-8"?>"#;

    /// The response a media resource sends for a `playcollect`, as captured.
    const PLAYCOLLECT_RESPONSE: &str = r#"<MediaServerControl version="1.0"><response playduration="11081" reason="timeout" playoffset="11081" text="The request has succeeded" request="playcollect" code="200" digits=""></response></MediaServerControl>"#;

    fn wrap(inner: &str) -> String {
        format!(r#"{DECL}<MediaServerControl version="1.0">{inner}</MediaServerControl>"#)
    }

    #[test]
    fn a_request_names_its_command_element_and_its_client_defined_id() {
        let body = wrap(
            r#"<request><play id="332985001"><prompt><audio url="file://a.wav"/></prompt></play></request>"#,
        );
        assert_eq!(
            request(&body),
            Some(Request { command: "play".into(), id: Some("332985001".into()) })
        );
    }

    #[test]
    fn a_request_without_an_id_reads_as_its_command_alone() {
        let body = wrap(
            r#"<request><playcollect maxdigits="1" barge="yes"><prompt><audio url="file://a.wav"/></prompt></playcollect></request>"#,
        );
        assert_eq!(request(&body), Some(Request { command: "playcollect".into(), id: None }));
    }

    /// XML admits either quote around a value, and whitespace around `=`.
    #[test]
    fn an_id_reads_in_either_quote_on_an_empty_element() {
        let single = wrap("<request><stop id='s-1'/></request>");
        assert_eq!(
            request(&single),
            Some(Request { command: "stop".into(), id: Some("s-1".into()) })
        );
        let spaced = wrap("<request>\r\n  <stop  id = \"s 2\" />\r\n</request>");
        assert_eq!(
            request(&spaced),
            Some(Request { command: "stop".into(), id: Some("s 2".into()) })
        );
    }

    /// Only the attribute NAMED `id` is the id: a longer name ending in it, or
    /// an `id` on the `<request>` wrapper, is not the command's.
    #[test]
    fn only_the_command_element_s_own_id_attribute_is_read() {
        let body = wrap(r#"<request id="wrapper"><play xid="x" uid='u'/></request>"#);
        assert_eq!(request(&body), Some(Request { command: "play".into(), id: None }));
    }

    /// A comment, a processing instruction and an attribute value holding `>`
    /// before the command element do not move the walk.
    #[test]
    fn the_walk_skips_comments_and_reads_through_a_quoted_angle_bracket() {
        let body = "<!-- lead --><?pi x?>\r\n<MediaServerControl version=\"1.0\" note=\"a>b\">\
                    <!-- inner --><request><playrecord id=\"r1\" dest=\"x>y\"/></request>\
                    </MediaServerControl>";
        assert_eq!(
            request(body),
            Some(Request { command: "playrecord".into(), id: Some("r1".into()) })
        );
    }

    #[test]
    fn a_response_a_notification_or_another_root_is_no_request() {
        assert_eq!(request(PLAYCOLLECT_RESPONSE), None);
        assert_eq!(request(&wrap(r#"<notification><keypress digit="1"/></notification>"#)), None);
        assert_eq!(request(r#"<other><request><play id="1"/></request></other>"#), None);
        assert_eq!(request(&wrap("<request/>")), None, "a request with no command names none");
        assert_eq!(request("not xml at all"), None);
    }

    #[test]
    fn a_response_names_the_command_type_it_answers() {
        assert_eq!(response_request(PLAYCOLLECT_RESPONSE), Some("playcollect"));
        assert_eq!(
            response_request(&wrap(r#"<response request='stop' code="200"/>"#)),
            Some("stop")
        );
        assert_eq!(response_request(&wrap(r#"<request><play id="1"/></request>"#)), None);
        assert_eq!(response_request(&wrap(r#"<notification><keypress/></notification>"#)), None);
        assert_eq!(response_request(&wrap(r#"<response code="400"/>"#)), None);
    }

    /// Inserted right after the element name; every other byte as it was.
    #[test]
    fn an_id_is_inserted_into_a_response_that_states_none() {
        let echoed = with_response_id(PLAYCOLLECT_RESPONSE, Some("332985001"));
        assert_eq!(
            echoed,
            PLAYCOLLECT_RESPONSE.replacen("<response ", "<response id=\"332985001\" ", 1)
        );
        assert_eq!(response_request(&echoed), Some("playcollect"));
    }

    #[test]
    fn a_stated_id_is_replaced_in_place_keeping_its_quote() {
        let body = wrap(r#"<response request="play" id='old-1' code="200" reason="stopped"/>"#);
        assert_eq!(
            with_response_id(&body, Some("live-7")),
            wrap(r#"<response request="play" id='live-7' code="200" reason="stopped"/>"#)
        );
    }

    /// A value holding the stated quote takes the other one, so the attribute
    /// stays well-formed.
    #[test]
    fn a_value_holding_the_quote_takes_the_other_quote() {
        let body = wrap(r#"<response request="play" id="old" code="200"/>"#);
        assert_eq!(
            with_response_id(&body, Some(r#"a"b"#)),
            wrap(r#"<response request="play" id='a"b' code="200"/>"#)
        );
        let bare = wrap(r#"<response request="play" code="200"/>"#);
        assert_eq!(
            with_response_id(&bare, Some(r#"a"b"#)),
            wrap(r#"<response id='a"b' request="play" code="200"/>"#)
        );
    }

    /// Removed with the whitespace that led it; a response stating none and
    /// asked for none is unchanged.
    #[test]
    fn a_stated_id_is_removed_where_the_request_carried_none() {
        let body = wrap(r#"<response request="stop"  id = "old" code="200"></response>"#);
        assert_eq!(
            with_response_id(&body, None),
            wrap(r#"<response request="stop" code="200"></response>"#)
        );
        assert_eq!(with_response_id(PLAYCOLLECT_RESPONSE, None), PLAYCOLLECT_RESPONSE);
    }

    #[test]
    fn a_body_holding_no_response_is_never_rewritten() {
        let notification = wrap(r#"<notification><keypress digit="1" id="k"/></notification>"#);
        assert_eq!(with_response_id(&notification, Some("x")), notification);
        let req = wrap(r#"<request><play id="1"/></request>"#);
        assert_eq!(with_response_id(&req, Some("x")), req);
        assert_eq!(with_response_id(&req, None), req);
    }

    #[test]
    fn the_media_type_is_read_by_token() {
        assert!(is_mscml("application/mediaservercontrol+xml"));
        assert!(is_mscml(" Application/MediaServerControl+XML ; charset=utf-8"));
        assert!(!is_mscml("application/xml"));
        assert!(!is_mscml("application/mediaservercontrol+xml-x"));
    }

    /// A bare MSCML body is one request; a multipart body yields each MSCML
    /// part's; any other type, or an empty body, yields none.
    #[test]
    fn requests_are_read_off_a_bare_body_and_off_each_mscml_part() {
        let play = wrap(r#"<request><play id="p1"/></request>"#);
        assert_eq!(
            requests_in(Some(CONTENT_TYPE), play.as_bytes()),
            vec![Request { command: "play".into(), id: Some("p1".into()) }]
        );
        assert!(requests_in(Some("application/xml"), play.as_bytes()).is_empty());
        assert!(requests_in(None, play.as_bytes()).is_empty());
        assert!(requests_in(Some(CONTENT_TYPE), b"").is_empty());

        let framed = format!(
            "--b1\r\nContent-Type: application/sdp\r\n\r\nv=0\r\n\r\n\
             --b1\r\nContent-Type: {CONTENT_TYPE}\r\n\r\n{play}\r\n--b1--\r\n"
        );
        assert_eq!(
            requests_in(Some("multipart/mixed;boundary=b1"), framed.as_bytes()),
            vec![Request { command: "play".into(), id: Some("p1".into()) }]
        );
    }
}
