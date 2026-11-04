//! The strings one message spec carries, each labelled by the part of the
//! message it sits in: every place an accessor may appear (§8.1).

use pivot_schema::body::Body;
use pivot_schema::msg::{MsgSpec, Ref};

/// Every string `msg` carries, with the part of the message it sits in.
pub(super) fn msg_strings(msg: &MsgSpec) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for h in &msg.headers {
        out.push(("header name".into(), h.name.clone()));
        out.push((format!("header {:?}", h.name), h.value.clone()));
    }
    for h in &msg.headers_present {
        out.push(("headers-present".into(), h.clone()));
    }
    for (what, r) in [("ruri", &msg.ruri), ("from", &msg.from), ("to", &msg.to)] {
        let Some(r) = r else { continue };
        match r {
            Ref::Positional(p) => {
                out.push((what.into(), p.pos.clone()));
                out.extend(p.form.iter().map(|form| (what.into(), form.clone())));
            }
            Ref::Frozen(f) => {
                out.push((what.into(), f.frozen.clone()));
                out.extend(f.kind.iter().map(|kind| (what.into(), kind.clone())));
            }
        }
        for text in r.addr().into_iter().chain(r.params()) {
            out.push((what.into(), text.to_string()));
        }
    }
    if let Some(contact) = &msg.contact {
        for text in contact.display.iter().chain(&contact.user) {
            out.push(("contact".into(), text.clone()));
        }
    }
    if let Some(body) = &msg.body {
        out.extend(body_strings(body).into_iter().map(|text| ("body".into(), text)));
    }
    out
}

/// Every string a body spec carries — refs, modes, content types.
fn body_strings(body: &Body) -> Vec<String> {
    match body {
        Body::Resource(r) => {
            let mut out = vec![r.reference.clone()];
            out.extend(r.rewrite.iter().cloned());
            out.extend(r.content_type.iter().cloned());
            out
        }
        Body::Shape(_) => Vec::new(),
        Body::Multipart(m) => {
            let mut out = vec![m.multipart.content_type.clone()];
            for p in &m.multipart.parts {
                out.push(p.content_type.clone());
                out.push(p.reference.clone());
                out.extend(p.rewrite.iter().cloned());
                out.extend(p.content_id.iter().cloned());
                for header in &p.headers {
                    out.push(header.name.clone());
                    out.push(header.value.clone());
                }
                out.extend(p.cid_linked.iter().cloned());
            }
            out
        }
    }
}
