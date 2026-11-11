//! One party's stream: the views of the binds a transaction ties into one user
//! agent ([`Parties`]), merged in event order.

use std::collections::{BTreeSet, HashMap};

use layer_harness::LaneKey;
use rfc_rules::rules::correlation::Parties;
use rfc_rules::Msg;

use super::view::BindView;

/// The bind views' messages in event order, each with its Call-ID and the bind
/// that recorded it: a datagram between two recorded binds stands twice.
fn in_event_order<'v>(views: &[&'v BindView]) -> Vec<(u64, &'v String, &'v Msg, &'v LaneKey)> {
    let mut rows: Vec<(u64, &String, &Msg, &LaneKey)> = views
        .iter()
        .flat_map(|v| {
            v.meta.iter().zip(&v.msgs).map(move |((seq, cid), m)| (*seq, cid, m, &v.bind))
        })
        .collect();
    rows.sort_by_key(|(seq, _, _, _)| *seq);
    rows
}

/// Per bind a party holds with another recorded bind, every bind of that party.
pub(super) fn joined(views: &[&BindView]) -> HashMap<LaneKey, BTreeSet<LaneKey>> {
    let binds: BTreeSet<&LaneKey> = views.iter().map(|v| &v.bind).collect();
    let parties = Parties::of(in_event_order(views).into_iter().map(|(_, _, m, _)| m));
    let mut out = HashMap::new();
    for set in parties.joined() {
        let held: BTreeSet<LaneKey> = set.into_iter().filter(|ep| binds.contains(ep)).collect();
        if held.len() < 2 {
            continue;
        }
        for bind in &held {
            out.insert(bind.clone(), held.clone());
        }
    }
    out
}

/// `own`'s view merged with the views of the binds of `members`, in event
/// order, under `own`'s bind. A datagram one member sent another stands once,
/// as its sender recorded it; the stamps rise strictly, as in one bind's view.
pub(super) fn merged(own: &BindView, members: &BTreeSet<LaneKey>, views: &[&BindView]) -> BindView {
    let party: Vec<&BindView> =
        views.iter().copied().filter(|v| members.contains(&v.bind)).collect();
    let mut view = BindView { bind: own.bind.clone(), msgs: Vec::new(), meta: Vec::new() };
    for (seq, cid, msg, recorded_at) in in_event_order(&party) {
        if msg.src != *recorded_at && members.contains(&msg.src) {
            continue;
        }
        let at_us = msg.at_us.max(view.msgs.last().map_or(0, |m| m.at_us + 1));
        view.msgs.push(Msg { at_us, ..msg.clone() });
        view.meta.push((seq, cid.clone()));
    }
    view
}
