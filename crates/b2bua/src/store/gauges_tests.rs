//! The store gauges a seeded call map publishes: the map lengths, the
//! state-machine cursor census and the per-call Vec census, pinned value by
//! value.

use std::sync::Arc;

use call::{Call, Dialog, DialogState, MachineId, PeerDialog, RequestRecord, StateLabel};

use super::{CallState, InMemoryCallStore};
use crate::config::B2buaConfig;
use crate::initial_invite::build_initial_call;
use crate::metrics::B2buaMetrics;
use crate::test_call::{invite, src};

/// A call of `w0`'s own on Call-ID `cid`, resting at `cursors`.
fn call(cid: &str, cursors: &[(&'static str, &'static str)]) -> Call {
    let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let mut ids = sip_ids::TurnIds::fixed(1);
    let mut call = build_initial_call(&invite("w0", "w1", cid), src(), &config, &mut ids, 0);
    call.sm_cursors.clear();
    for (machine, state) in cursors {
        call.sm_cursors.insert(MachineId::new(machine), StateLabel::new(state));
    }
    call
}

/// A dialog holding `requests` open requests, `routes` route-set entries and
/// `fronts` fronted callee dialogs.
fn dialog(tag: &str, requests: usize, routes: usize, fronts: usize) -> Dialog {
    let mut d = Dialog::new(tag, format!("{tag}-peer"), DialogState::Confirmed, 1);
    d.requests = (0..requests).map(|n| RequestRecord::relayed("INFO", 2, n as i64 + 2)).collect();
    d.route_set = (0..routes).map(|n| format!("<sip:p{n}.example.com;lr>")).collect();
    d.fronts = (0..fronts)
        .map(|n| PeerDialog { leg_id: format!("b{n}"), peer_tag: format!("bt{n}") })
        .collect();
    d
}

fn line(text: &str, name: &str) -> String {
    text.lines()
        .find(|l| l.starts_with(name) && !l.starts_with('#'))
        .unwrap_or_else(|| panic!("no {name} sample in\n{text}"))
        .to_string()
}

/// Three seeded calls publish the map lengths, every cursor they rest at,
/// and the sums and the per-call maximum of what they hold.
#[test]
fn a_seeded_store_publishes_its_census() {
    let metrics = B2buaMetrics::new();
    let state = CallState::new(Arc::new(InMemoryCallStore::new()), "w0", metrics.clone());

    let mut a = call("census-a", &[("global-call", "Active"), ("transfer", "CRinging")]);
    a.a_leg.dialogs.insert(dialog("a1", 2, 2, 1));
    let mut b_leg = a.a_leg.clone();
    b_leg.dialogs.insert(dialog("ab1", 1, 1, 0));
    a.b_legs.push(b_leg);
    let base_timers = a.timers.len();
    a.timers.extend(a.timers.clone());
    let mut b = call("census-b", &[("global-call", "Active")]);
    b.a_leg.dialogs.insert(dialog("b1", 4, 0, 2));
    let c = call("census-c", &[("global-call", "Terminating")]);
    let base_cdr = a.cdr_events.len() + b.cdr_events.len() + c.cdr_events.len();
    let base_dialogs = 4 * c.a_leg.dialogs.len();
    for call in [a, b, c] {
        state.create(call, None);
    }

    state.sample_store_gauges();
    let text = metrics.prometheus_text();

    assert_eq!(line(&text, "b2bua_store_calls "), "b2bua_store_calls 3");
    assert_eq!(line(&text, "b2bua_store_touched "), "b2bua_store_touched 3");
    assert_eq!(line(&text, "b2bua_store_takeover_at "), "b2bua_store_takeover_at 0");
    for cursor in [
        "b2bua_sm_cursors{machine=\"global-call\",state=\"Active\"} 2",
        "b2bua_sm_cursors{machine=\"global-call\",state=\"Terminating\"} 1",
        "b2bua_sm_cursors{machine=\"transfer\",state=\"CRinging\"} 1",
    ] {
        assert!(text.contains(cursor), "{cursor} in\n{text}");
    }
    assert_eq!(text.matches("b2bua_sm_cursors{").count(), 3);
    let cdr = format!("b2bua_census_cdr_events {base_cdr}");
    assert_eq!(line(&text, "b2bua_census_cdr_events "), cdr);
    // Calls a and b hold 2 + 2 + 1 and 4 requests: 9 in all, 5 at most.
    assert_eq!(line(&text, "b2bua_census_pending_requests "), "b2bua_census_pending_requests 9");
    assert_eq!(
        line(&text, "b2bua_census_pending_requests_max "),
        "b2bua_census_pending_requests_max 5"
    );
    // a's a-leg dialog and its copy on the b-leg, the b-leg's own, and b's.
    let dialogs = format!("b2bua_census_dialogs {}", base_dialogs + 4);
    assert_eq!(line(&text, "b2bua_census_dialogs "), dialogs);
    assert_eq!(line(&text, "b2bua_census_route_set "), "b2bua_census_route_set 5");
    let timers = format!("b2bua_census_timers {}", 4 * base_timers);
    assert_eq!(line(&text, "b2bua_census_timers "), timers);
    // Fronts count on the a-leg only: a's 1 and b's 2.
    assert_eq!(line(&text, "b2bua_census_tag_map "), "b2bua_census_tag_map 3");
    assert_eq!(line(&text, "b2bua_census_b_legs "), "b2bua_census_b_legs 1");
}
