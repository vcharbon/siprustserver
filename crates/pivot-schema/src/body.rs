//! Message bodies (`PCAP2TEST_PIVOT_V3.md` §8.3).
//!
//! Three shapes, and a body is exactly one of them: a resource the send emits,
//! a declared shape the expect checks, or a decomposed multipart. **Splitting a
//! multipart body is EXTRACTION's job** — the flows emitter writes each part out
//! with its content-type, its entity headers and its payload, so no consumer
//! here owns MIME and the pivot only references files that already exist.
//!
//! A body is emitted as the document holds it: every part's payload byte-exact,
//! its `Content-ID` and its entity headers verbatim (RFC 2045 §3). SDP is the
//! ONE content the tool rewrites, because replay rebooks addresses and ports.
//!
//! Per-part handling is a content-type registry, exported by [`crate::tiers`].

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The body a step emits or checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Body {
    /// A decomposed multipart body: the parts are sibling resource files.
    Multipart(MultipartBody),
    /// A single body stored as a resource file.
    Resource(ResourceBody),
    /// A declared shape, checked rather than emitted.
    Shape(ShapeBody),
}

/// A single body carried by a resource file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResourceBody {
    /// Path of the resource file, relative to the case directory.
    #[serde(rename = "ref")]
    pub reference: String,
    /// Rewrite tokens the lane applies before emission (e.g. `c=addr`,
    /// `m=port` on SDP). Omitted where the body replays byte-exact.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rewrite: Vec<String>,
    /// Handling mode, on a body the registry freezes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<BodyMode>,
    /// The captured `Content-Type` value, VERBATIM and with its parameters, so
    /// the body replays under the type the wire wrote. Omitted only where
    /// render derives exactly it — a rewritten body whose type is bare
    /// `application/sdp` — so the stored and derived values cannot drift.
    #[serde(rename = "content-type", default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
}

/// A body asserted by SHAPE rather than content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ShapeBody {
    /// Which shape the expectation asserts.
    pub mode: BodyShape,
}

/// The declared shape of an expected body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum BodyShape {
    /// An SDP body is present; its content is not asserted here.
    SdpPresent,
    /// The message carries no body, and that IS the assertion (RFC 3264 §5: a
    /// bodyless INVITE is the delayed offer, not an unclassified body).
    Absent,
    /// A multipart body is present.
    MultipartPresent,
}

/// How the registry handles a stored body or part.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum BodyMode {
    /// Replayed byte-exact; the payload is text.
    Frozen,
    /// Replayed byte-exact; the payload is binary.
    FrozenBinary,
}

/// A multipart body, referencing its already-decomposed parts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MultipartBody {
    /// The container and its parts.
    pub multipart: Multipart,
}

/// The multipart container and its parts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Multipart {
    /// The container's own media type as the wire wrote it, MINUS its
    /// `boundary` parameter: the boundary — and only the boundary — is
    /// regenerated at render, so every other parameter rides through.
    #[serde(rename = "content-type")]
    pub content_type: String,
    /// The parts, in wire order.
    pub parts: Vec<Part>,
}

/// One MIME part with its per-content-type handling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Part {
    /// The part's `Content-Type` value, VERBATIM and with its parameters —
    /// the composer writes this line back unchanged. The body registry (§8.2)
    /// matches on the bare type, so a parameter never changes the handling.
    #[serde(rename = "content-type")]
    pub content_type: String,
    /// Path of the part's resource file, relative to the case directory.
    #[serde(rename = "ref")]
    pub reference: String,
    /// Rewrite tokens the lane applies to this part before emission.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rewrite: Vec<String>,
    /// Handling mode, on a part the registry freezes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<BodyMode>,
    /// The part's OWN `Content-ID` value, angle brackets as the wire wrote them
    /// (RFC 2045 §7). It is what a `cid:` reference resolves against (RFC 5621
    /// §3), so a part any header points at replays under the same id.
    #[serde(rename = "content-id", default, skip_serializing_if = "Option::is_none")]
    pub content_id: Option<String>,
    /// The part's remaining entity headers, verbatim and in wire order —
    /// `Content-Transfer-Encoding`, `Content-Disposition`, whatever else the
    /// part states. `Content-Type` and `Content-ID` are held in their own
    /// fields and are never repeated here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<EntityHeader>,
    /// Header names (lowercased) that reference this part by `cid:`. COMPUTED
    /// from the part's `Content-ID` and the message's header list — it needs no
    /// MIME parser, only the part id and the names.
    #[serde(rename = "cid-linked", default, skip_serializing_if = "Vec::is_empty")]
    pub cid_linked: Vec<String>,
}

/// One entity header of a MIME part, exactly as the part carried it (RFC 2045
/// §3). A part header states MIME, never the origin platform's vocabulary, so
/// unlike a message header (`crate::msg::Header`) it carries no check class.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EntityHeader {
    /// Header name, in the part's own spelling and casing.
    pub name: String,
    /// Header value, verbatim.
    pub value: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Body {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn each_body_shape_decodes_to_its_own_variant() {
        assert!(matches!(
            parse(r#"{"ref":"resources/s01_uac1_0.sdp","rewrite":["c=addr","m=port"]}"#),
            Body::Resource(_)
        ));
        assert!(matches!(
            parse(r#"{"ref":"resources/s11_uas1_0.xml","mode":"frozen","content-type":"application/mscp+xml"}"#),
            Body::Resource(_)
        ));
        assert!(matches!(parse(r#"{"mode":"absent"}"#), Body::Shape(_)));
        assert!(matches!(parse(r#"{"mode":"sdp-present"}"#), Body::Shape(_)));
        assert!(matches!(
            parse(r#"{"multipart":{"content-type":"multipart/mixed","parts":[]}}"#),
            Body::Multipart(_)
        ));
    }

    #[test]
    fn a_shape_body_and_a_resource_body_cannot_be_confused() {
        // `frozen` is a resource handling mode, never a declared shape: without
        // the ref there is nothing to freeze.
        assert!(serde_json::from_str::<Body>(r#"{"mode":"frozen"}"#).is_err());
        // `sdp-present` is a shape, so it cannot ride on a resource body.
        assert!(serde_json::from_str::<Body>(r#"{"ref":"r.sdp","mode":"sdp-present"}"#).is_err());
    }

    #[test]
    fn a_part_round_trips_with_its_cid_links() {
        let text = r#"{"content-type":"application/EmergencyCallData.eCall.MSD","ref":"resources/s01_uac1_1.bin","mode":"frozen-binary","cid-linked":["call-info"]}"#;
        let part: Part = serde_json::from_str(text).unwrap();
        assert_eq!(part.mode, Some(BodyMode::FrozenBinary));
        assert_eq!(part.cid_linked, ["call-info"]);
        assert_eq!(serde_json::to_string(&part).unwrap(), text);
    }

    /// A content type is STORED, never parsed: emission writes the stored value
    /// back as the `Content-Type` line, so its MIME parameters have to survive
    /// the round trip untouched (§8.3). Both corpus spellings are real ones.
    #[test]
    fn a_content_type_keeps_its_mime_parameters() {
        let text = r#"{"content-type":"application/pidf+xml;charset=utf-8","ref":"resources/s01_uac1_2.xml","mode":"frozen"}"#;
        let part: Part = serde_json::from_str(text).unwrap();
        assert_eq!(part.content_type, "application/pidf+xml;charset=utf-8");
        assert_eq!(serde_json::to_string(&part).unwrap(), text);

        let Body::Resource(resource) = parse(
            r#"{"ref":"resources/s07_uas1_0.txt","mode":"frozen","content-type":"message/sipfrag;version=2.0"}"#,
        ) else {
            panic!("a single body is a resource body")
        };
        assert_eq!(resource.content_type.as_deref(), Some("message/sipfrag;version=2.0"));
    }

    /// A part replays under its own `Content-ID` and its own entity headers, so
    /// both survive the round trip in the order the wire wrote them.
    #[test]
    fn a_part_round_trips_with_its_content_id_and_entity_headers() {
        let text = r#"{"content-type":"application/vnd.example.indata","ref":"resources/s01_uac1_1.bin","mode":"frozen-binary","content-id":"<indata@example.invalid>","headers":[{"name":"Content-Transfer-Encoding","value":"binary"},{"name":"Content-Disposition","value":"signal;handling=optional"}]}"#;
        let part: Part = serde_json::from_str(text).unwrap();
        assert_eq!(part.content_id.as_deref(), Some("<indata@example.invalid>"));
        assert_eq!(
            part.headers.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(),
            ["Content-Transfer-Encoding", "Content-Disposition"]
        );
        assert_eq!(serde_json::to_string(&part).unwrap(), text);
    }
}
