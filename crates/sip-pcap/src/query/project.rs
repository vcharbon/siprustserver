//! Projecting a matched call group onto named fields.
//!
//! One field vocabulary serves two jobs, deliberately: it is what a summary
//! row contains, AND what [`Neighbours::key`] means by "similar". So "show me
//! the callee" and "find the other calls to that callee" name the same thing
//! the same way, and a new field serves both without a second language.

use serde_json::{json, Value};
use sip_message::header::Uri;

use crate::flow::{CallGroup, Flows, MatchEvidence};

use super::ast::{KeyField, Neighbours};
use super::eval::{group_t0, group_t1};

/// One matched group as the named fields, in the order asked for.
pub fn summary_row(flows: &Flows, group_idx: usize, fields: &[KeyField]) -> Value {
    let group = &flows.groups[group_idx];
    let mut obj = serde_json::Map::new();
    for f in fields {
        obj.insert(f.as_str().to_string(), field(flows, group_idx, group, *f));
    }
    Value::Object(obj)
}

/// The neighbour key of a group: the key fields joined into one opaque string.
/// Two groups are "the same" exactly when this matches.
pub fn neighbour_key(flows: &Flows, group_idx: usize, spec: &Neighbours) -> String {
    let group = &flows.groups[group_idx];
    spec.key
        .iter()
        .map(|f| match field(flows, group_idx, group, *f) {
            Value::String(s) => s,
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\u{1f}")
}

/// Expand hits into the groups that resemble them: same key, first activity
/// within the window, the hit itself excluded. Ordered by distance from the
/// hit, so a `max` cut keeps the nearest.
pub fn neighbours_of(flows: &Flows, hits: &[usize], spec: &Neighbours) -> Vec<(usize, Vec<usize>)> {
    let keys: Vec<String> = (0..flows.groups.len())
        .map(|g| neighbour_key(flows, g, spec))
        .collect();
    let t0s: Vec<u64> = flows.groups.iter().map(|g| group_t0(flows, g)).collect();

    hits.iter()
        .map(|&hit| {
            let mut near: Vec<(u64, usize)> = (0..flows.groups.len())
                .filter(|&g| g != hit && keys[g] == keys[hit])
                .map(|g| (t0s[g].abs_diff(t0s[hit]), g))
                .filter(|(dt, _)| *dt <= spec.window_us)
                .collect();
            near.sort_unstable();
            if spec.max > 0 {
                near.truncate(spec.max);
            }
            (hit, near.into_iter().map(|(_, g)| g).collect())
        })
        .collect()
}

/// A group's value for one field. Leg-scoped fields yield the value of each
/// leg, in group order — a call has more than one Call-ID and saying so is
/// the point.
fn field(flows: &Flows, group_idx: usize, group: &CallGroup, f: KeyField) -> Value {
    let legs = || group.legs.iter().map(|&l| &flows.legs[l]);
    match f {
        KeyField::Group => json!(group_idx),
        KeyField::T0Us => json!(group_t0(flows, group)),
        KeyField::DurMs => {
            json!(group_t1(flows, group).saturating_sub(group_t0(flows, group)) / 1_000)
        }
        KeyField::LegCount => json!(group.legs.len()),
        KeyField::CallId => json!(legs().map(|l| l.call_id.clone()).collect::<Vec<_>>()),
        KeyField::Ruri => json!(invite_field(flows, group, |inv| inv.ruri.text().into_owned())),
        KeyField::RuriUser => json!(invite_field(flows, group, |inv| uri_user(&inv.ruri))),
        KeyField::FromUri => {
            json!(invite_field(flows, group, |inv| inv.from_uri.text().into_owned()))
        }
        KeyField::ToUri => json!(invite_field(flows, group, |inv| inv.to_uri.text().into_owned())),
        KeyField::FromUser => json!(invite_field(flows, group, |inv| uri_user(&inv.from_uri))),
        KeyField::ToUser => json!(invite_field(flows, group, |inv| uri_user(&inv.to_uri))),
        KeyField::Src => json!(legs()
            .map(|l| l.msgs.first().map(|m| m.src.to_string()))
            .collect::<Vec<_>>()),
        KeyField::Dst => json!(legs()
            .map(|l| l.msgs.first().map(|m| m.dst.to_string()))
            .collect::<Vec<_>>()),
        KeyField::FinalStatus => json!(legs().map(|l| l.final_status).collect::<Vec<_>>()),
        KeyField::Saw180 => json!(legs().any(|l| l.saw_180)),
        KeyField::TerminatedBy => {
            json!(legs().find_map(|l| l.terminated_by).map(|t| t.as_str()))
        }
        KeyField::MsgCount => json!(legs().map(|l| l.msgs.len()).sum::<usize>()),
        KeyField::Hops => json!(legs()
            .flat_map(|l| l.hops.iter().map(|h| format!("{}|{}", h.a, h.b)))
            .collect::<Vec<_>>()),
        KeyField::Evidence => json!(group
            .evidence
            .iter()
            .map(evidence_kind)
            .collect::<Vec<_>>()),
        KeyField::AsSocket => json!(group
            .evidence
            .iter()
            .filter_map(|e| match e {
                MatchEvidence::DerivedCallId { as_socket, .. } => Some(as_socket.to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()),
        KeyField::PeerSocket => json!(group
            .evidence
            .iter()
            .filter_map(|e| match e {
                MatchEvidence::DerivedCallId { peer_socket, .. } => Some(peer_socket.to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()),
    }
}

fn invite_field(
    flows: &Flows,
    group: &CallGroup,
    pick: impl Fn(&crate::flow::InviteSummary) -> String,
) -> Vec<Option<String>> {
    group.legs.iter().map(|&l| flows.legs[l].invite.as_ref().map(&pick)).collect()
}

/// The user a URI names — the dialed number, host-, scheme- and
/// param-insensitive, so grouping by callee is not defeated by a rewritten
/// host or a `tel:`⇄`sip:` swap. A URI naming no user yields its host, which
/// is the whole value for input no reader accepts.
fn uri_user(uri: &Uri) -> String {
    uri.user_identity().unwrap_or_else(|| uri.host().to_string())
}

fn evidence_kind(e: &MatchEvidence) -> &'static str {
    match e {
        MatchEvidence::SharedToken { .. } => "shared_token",
        MatchEvidence::SharedHeaderParam { .. } => "shared_header_param",
        MatchEvidence::DerivedCallId { .. } => "derived_call_id",
        MatchEvidence::IdentityAdjacency { .. } => "identity_adjacency",
    }
}
