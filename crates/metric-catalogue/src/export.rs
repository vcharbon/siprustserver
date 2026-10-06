//! The catalogues as JSON, the contract a reader of the metrics is checked
//! against: deterministic, in catalogue order.
//!
//! ```json
//! {"catalogues": [{"binary": "...", "families": [
//!   {"name": "...", "kind": "counter|gauge|histogram",
//!    "labels": [[{"name": "...", "values": ["..."]}]],
//!    "semi_open": false, "cap": null, "capped_labels": [], "overflow": null,
//!    "help": "..."}]}]}
//! ```
//!
//! `labels` lists the blocks of label dimensions, each dimension with its
//! declared values; a family without labels has one empty block. `cap`,
//! `capped_labels` (the labels whose values are unbounded) and `overflow`
//! (the overflow counter's name) are set on a capped family.

use std::fmt::Write;

use crate::catalogue::Catalogue;
use crate::family::Family;

/// The JSON of `catalogues`, one line per family.
pub fn to_json(catalogues: &[Catalogue]) -> String {
    let mut s = String::from("{\"catalogues\": [");
    for (i, c) in catalogues.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str("\n  {\"binary\": ");
        string(&mut s, c.binary);
        s.push_str(", \"families\": [");
        for (j, f) in c.families().enumerate() {
            if j > 0 {
                s.push(',');
            }
            s.push_str("\n    ");
            family(&mut s, f);
        }
        s.push_str("\n  ]}");
    }
    s.push_str("\n]}\n");
    s
}

fn family(s: &mut String, f: &Family) {
    s.push_str("{\"name\": ");
    string(s, f.name);
    let _ = write!(s, ", \"kind\": \"{}\", \"labels\": [", f.kind.as_str());
    for (b, dims) in f.labels.blocks().iter().enumerate() {
        if b > 0 {
            s.push_str(", ");
        }
        s.push('[');
        for (d, dim) in dims.iter().enumerate() {
            if d > 0 {
                s.push_str(", ");
            }
            s.push_str("{\"name\": ");
            string(s, dim.name);
            s.push_str(", \"values\": [");
            for (v, value) in dim.values.iter().enumerate() {
                if v > 0 {
                    s.push_str(", ");
                }
                string(s, value);
            }
            s.push_str("]}");
        }
        s.push(']');
    }
    let _ = write!(s, "], \"semi_open\": {}, \"cap\": ", f.is_semi_open());
    match f.cap() {
        Some(cap) => {
            let _ = write!(s, "{}, \"capped_labels\": [", cap.max);
            for (i, label) in cap.labels.iter().enumerate() {
                if i > 0 {
                    s.push_str(", ");
                }
                string(s, label);
            }
            s.push_str("], \"overflow\": ");
            string(s, cap.overflow.name);
        }
        None => s.push_str("null, \"capped_labels\": [], \"overflow\": null"),
    }
    s.push_str(", \"help\": ");
    string(s, f.help);
    s.push('}');
}

/// A JSON string literal: quote, backslash and control characters escaped.
fn string(s: &mut String, v: &str) {
    s.push('"');
    for c in v.chars() {
        match c {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            '\n' => s.push_str("\\n"),
            c if (c as u32) < 0x20 => {
                let _ = write!(s, "\\u{:04x}", c as u32);
            }
            c => s.push(c),
        }
    }
    s.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Dim, Labels};

    const OVER: Family = Family::counter("p_overflow_total", Labels::None, "past the cap");
    const PEER: Family =
        Family::counter("p_total", Labels::Product(&[Dim::new("peer", &[])]), "by \"peer\"")
            .capped(&OVER, &["peer"]);
    const H: Family = Family::histogram(
        "h_seconds",
        Labels::Union(&[&[Dim::new("a", &["x"])], &[Dim::new("b", &["y", "z"])]]),
        "h",
    );

    /// The shape a reader parses: pinned byte for byte.
    #[test]
    fn the_export_shape_is_pinned() {
        let cat = Catalogue { binary: "bin", sections: &[&[PEER, OVER, H]] };
        assert_eq!(
            to_json(&[cat]),
            "{\"catalogues\": [\n  {\"binary\": \"bin\", \"families\": [\n    \
             {\"name\": \"p_total\", \"kind\": \"counter\", \"labels\": [[{\"name\": \"peer\", \"values\": []}]], \
             \"semi_open\": true, \"cap\": 256, \"capped_labels\": [\"peer\"], \"overflow\": \"p_overflow_total\", \
             \"help\": \"by \\\"peer\\\"\"},\n    \
             {\"name\": \"p_overflow_total\", \"kind\": \"counter\", \"labels\": [[]], \"semi_open\": false, \
             \"cap\": null, \"capped_labels\": [], \"overflow\": null, \"help\": \"past the cap\"},\n    \
             {\"name\": \"h_seconds\", \"kind\": \"histogram\", \"labels\": [[{\"name\": \"a\", \"values\": [\"x\"]}], \
             [{\"name\": \"b\", \"values\": [\"y\", \"z\"]}]], \"semi_open\": false, \"cap\": null, \
             \"capped_labels\": [], \"overflow\": null, \"help\": \"h\"}\n  ]}\n]}\n"
        );
    }
}
