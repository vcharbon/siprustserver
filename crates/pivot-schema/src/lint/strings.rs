//! Every string position a document carries that could hold a `${…}`.
//!
//! ONE inventory, two consumers: the accessor validator resolves what it finds,
//! and the generator-subset gate refuses that a capture carries any of it. They
//! read the same list on purpose — the first version of this check scanned
//! header values only, so an accessor hidden in a frozen tier-2 ref passed both
//! gates. A field added to a message, a deviation or a postcondition must be
//! added here, and the tests that walk a fully-populated one are what say so.

use crate::body::Body;
use crate::check::Check;
use crate::deviation::Deviation;
use crate::flow::{FlowNode, Step};
use crate::msg::{MsgSpec, Ref};
use crate::postcondition::{CdrExpectation, Postconditions};

/// One scannable string: where it is, and what it says.
pub(super) struct Text<'a> {
    /// Field path relative to the node, for the diagnostic.
    pub field: String,
    pub text: &'a str,
}

fn one<'a>(field: impl Into<String>, text: &'a str) -> Text<'a> {
    Text { field: field.into(), text }
}

/// Every string a flow node carries. An `alt` or `unordered` block contributes
/// nothing itself — its steps are visited in their own right.
pub(super) fn node_strings(node: &FlowNode) -> Vec<Text<'_>> {
    match node {
        FlowNode::Inject(inject) => {
            let mut out = vec![one("action", inject.action.as_str())];
            if let Some(target) = &inject.target {
                out.push(one("target", target.as_str()));
            }
            out
        }
        FlowNode::Message(_) | FlowNode::Alt(_) | FlowNode::Unordered(_) => Vec::new(),
    }
}

/// Every string a message step carries.
pub(super) fn step_strings(step: &Step) -> Vec<Text<'_>> {
    let mut out = Vec::new();
    for check in &step.checks {
        out.extend(check_strings("checks", check));
    }
    out.extend(msg_strings(&step.msg));
    out
}

/// Every string a check carries. A `field` selector interpolates like a value
/// does — `header(${…})` is a legal selector — so both halves are scanned.
fn check_strings<'a>(at: &str, check: &'a Check) -> Vec<Text<'a>> {
    let mut out = vec![one(format!("{at}[].field"), check.field.as_str())];
    if let Some(value) = &check.value {
        out.push(one(format!("{at}[].value"), value.as_str()));
    }
    out
}

/// Every string the postconditions carry, settle-time checks and CDR checks
/// alike.
pub(super) fn postcondition_strings(postconditions: &Postconditions) -> Vec<Text<'_>> {
    let mut out = Vec::new();
    for check in &postconditions.checks {
        out.extend(check_strings("checks", check));
    }
    if let Some(CdrExpectation::Expected(cdr)) = &postconditions.cdr {
        for check in &cdr.checks {
            out.extend(check_strings("cdr.checks", check));
        }
    }
    if let Some(CdrExpectation::Absent(absent)) = &postconditions.cdr {
        out.push(one("cdr.absent", absent.absent.as_str()));
    }
    out
}

/// Every string a deviation carries. Its ids are resolved as references
/// elsewhere; they are scanned here too, because an accessor is refused in
/// EVERY string a document carries and an id is no exception.
pub(super) fn deviation_strings(deviation: &Deviation) -> Vec<Text<'_>> {
    let mut out = vec![one("id", deviation.id.as_str()), one("kind", deviation.kind.as_str())];
    for (field, value) in [
        ("leg", &deviation.leg),
        ("step", &deviation.step),
        ("header", &deviation.header),
        ("races", &deviation.races),
    ] {
        if let Some(value) = value {
            out.push(one(field, value.as_str()));
        }
    }
    out.extend(deviation.preserve.iter().map(|p| one("preserve[]", p.as_str())));
    out
}

fn msg_strings(msg: &MsgSpec) -> Vec<Text<'_>> {
    let mut out = Vec::new();
    for (field, value) in [
        ("msg.method", &msg.method),
        ("msg.reason", &msg.reason),
        ("msg.cseq-method", &msg.cseq_method),
    ] {
        if let Some(value) = value {
            out.push(one(field, value.as_str()));
        }
    }
    for (field, reference) in
        [("msg.ruri", &msg.ruri), ("msg.from", &msg.from), ("msg.to", &msg.to)]
    {
        let Some(reference) = reference else { continue };
        match reference {
            Ref::Positional(positional) => {
                out.push(one(format!("{field}.pos"), positional.pos.as_str()));
                if let Some(form) = &positional.form {
                    out.push(one(format!("{field}.form"), form.as_str()));
                }
            }
            Ref::Frozen(frozen) => {
                out.push(one(format!("{field}.frozen"), frozen.frozen.as_str()));
                if let Some(kind) = &frozen.kind {
                    out.push(one(format!("{field}.kind"), kind.as_str()));
                }
            }
        }
    }
    for header in &msg.headers {
        out.push(one("msg.headers[].name", header.name.as_str()));
        out.push(one("msg.headers[].value", header.value.as_str()));
    }
    for present in &msg.headers_present {
        out.push(one("msg.headers-present[]", present.as_str()));
    }
    out.extend(body_strings(msg.body.as_ref()));
    out
}

fn body_strings(body: Option<&Body>) -> Vec<Text<'_>> {
    let mut out = Vec::new();
    match body {
        Some(Body::Resource(resource)) => {
            out.push(one("msg.body.ref", resource.reference.as_str()));
            out.extend(resource.rewrite.iter().map(|r| one("msg.body.rewrite[]", r.as_str())));
            if let Some(content_type) = &resource.content_type {
                out.push(one("msg.body.content-type", content_type.as_str()));
            }
        }
        Some(Body::Multipart(multipart)) => {
            out.push(one("msg.body.multipart.content-type", multipart.multipart.content_type.as_str()));
            for part in &multipart.multipart.parts {
                out.push(one("msg.body.multipart.parts[].content-type", part.content_type.as_str()));
                out.push(one("msg.body.multipart.parts[].ref", part.reference.as_str()));
                out.extend(
                    part.rewrite
                        .iter()
                        .map(|r| one("msg.body.multipart.parts[].rewrite[]", r.as_str())),
                );
                if let Some(content_id) = &part.content_id {
                    out.push(one("msg.body.multipart.parts[].content-id", content_id.as_str()));
                }
                for header in &part.headers {
                    out.push(one("msg.body.multipart.parts[].headers[].name", header.name.as_str()));
                    out.push(one(
                        "msg.body.multipart.parts[].headers[].value",
                        header.value.as_str(),
                    ));
                }
                out.extend(
                    part.cid_linked
                        .iter()
                        .map(|c| one("msg.body.multipart.parts[].cid-linked[]", c.as_str())),
                );
            }
        }
        Some(Body::Shape(_)) | None => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A step carrying every field a step can carry. The test below asserts the
    /// inventory reaches all of them, which is what stops a field added to
    /// [`MsgSpec`] from becoming a place an accessor hides.
    fn fully_populated() -> Step {
        serde_json::from_str(
            r#"{
              "id": "s1", "leg": "A", "op": "expect", "check": "assert",
              "checks": [{ "field": "to.tag", "op": "eq", "value": "v" }],
              "msg": {
                "method": "INVITE", "status": 200, "reason": "OK", "cseq-method": "INVITE",
                "ruri": { "pos": "called[0][0]", "form": "trunk-composed" },
                "from": { "frozen": "f", "kind": "anonymous" },
                "to": { "pos": "caller" },
                "headers": [{ "name": "Allow", "value": "INVITE" }],
                "headers-present": ["session-expires"],
                "body": { "multipart": { "content-type": "multipart/mixed", "parts": [
                  { "content-type": "application/sdp", "ref": "r.sdp",
                    "rewrite": ["c=addr"], "content-id": "<offer@x.invalid>",
                    "headers": [{ "name": "Content-Disposition", "value": "session" }],
                    "cid-linked": ["call-info"] }
                ] } }
              },
              "delay": { "ms": 0, "from": "trigger", "compressible": true, "timer_linked": false }
            }"#,
        )
        .expect("a step")
    }

    #[test]
    fn the_inventory_reaches_every_string_a_step_can_carry() {
        let step = fully_populated();
        let found: Vec<String> = step_strings(&step).into_iter().map(|t| t.field).collect();
        for field in [
            "checks[].field",
            "checks[].value",
            "msg.method",
            "msg.reason",
            "msg.cseq-method",
            "msg.ruri.pos",
            "msg.ruri.form",
            "msg.from.frozen",
            "msg.from.kind",
            "msg.to.pos",
            "msg.headers[].name",
            "msg.headers[].value",
            "msg.headers-present[]",
            "msg.body.multipart.content-type",
            "msg.body.multipart.parts[].content-type",
            "msg.body.multipart.parts[].ref",
            "msg.body.multipart.parts[].rewrite[]",
            "msg.body.multipart.parts[].content-id",
            "msg.body.multipart.parts[].headers[].name",
            "msg.body.multipart.parts[].headers[].value",
            "msg.body.multipart.parts[].cid-linked[]",
        ] {
            assert!(found.iter().any(|f| f == field), "{field} is not in the inventory: {found:?}");
        }
    }

    #[test]
    fn a_single_resource_body_contributes_its_ref_and_media_type() {
        let step: Step = serde_json::from_str(
            r#"{"id":"s1","leg":"A","op":"send",
                "msg":{"method":"INVITE","body":{"ref":"r.xml","mode":"frozen","content-type":"application/x"}},
                "delay":{"ms":0,"from":"trigger","compressible":true,"timer_linked":false}}"#,
        )
        .expect("a step");
        let found: Vec<&str> = step_strings(&step).iter().map(|t| t.text).collect();
        assert!(found.contains(&"r.xml") && found.contains(&"application/x"), "{found:?}");
    }

    #[test]
    fn the_inventory_reaches_every_string_a_deviation_can_carry() {
        let deviation: Deviation = serde_json::from_str(
            r#"{"id":"d1","kind":"malformed-header","leg":"B","step":"s8","header":"Refer-To",
                "preserve":["header-value"],"races":"s9","retransmits":2,
                "value":{"from":"${step:s7.cseq}","delta":1}}"#,
        )
        .expect("a deviation");
        let found: Vec<String> = deviation_strings(&deviation).into_iter().map(|t| t.field).collect();
        for field in ["id", "kind", "leg", "step", "header", "races", "preserve[]"] {
            assert!(found.iter().any(|f| f == field), "{field} is not in the inventory: {found:?}");
        }
    }

    #[test]
    fn the_inventory_reaches_a_postcondition_check_field_as_well_as_its_value() {
        let postconditions: Postconditions = serde_json::from_str(
            r#"{"cdr":{"count":1,"checks":[{"field":"events","op":"eq","value":"Answer"}]},
                "checks":[{"field":"orphans_total","op":"eq","value":"0"}]}"#,
        )
        .expect("postconditions");
        let found: Vec<String> =
            postcondition_strings(&postconditions).into_iter().map(|t| t.field).collect();
        for field in ["checks[].field", "checks[].value", "cdr.checks[].field", "cdr.checks[].value"]
        {
            assert!(found.iter().any(|f| f == field), "{field} is not in the inventory: {found:?}");
        }
        let absent: Postconditions =
            serde_json::from_str(r#"{"cdr":{"absent":"capture-carries-no-cdr"}}"#).expect("absence");
        assert_eq!(
            postcondition_strings(&absent).into_iter().map(|t| t.text).collect::<Vec<_>>(),
            ["capture-carries-no-cdr"]
        );
    }

    #[test]
    fn an_injection_contributes_its_action_and_target() {
        let node: FlowNode = serde_json::from_str(
            r#"{"id":"i1","op":"inject","action":"node-kill","target":"b"}"#,
        )
        .expect("a node");
        let found: Vec<&str> = node_strings(&node).iter().map(|t| t.text).collect();
        assert_eq!(found, ["node-kill", "b"]);
    }
}
