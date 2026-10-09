//! The compact wire ladder of a run — one line per recorded message, sender →
//! receiver, relative timestamps — the first section of a failure dump.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use layer_harness::{Channel, Recorder};
use sip_net::{to_sip_entries, SignalingNetworkEvent};

use crate::report::wire::{facets, format_relative};

/// The ladder of every message `channel` recorded, each party named from the
/// `recorder`'s lanes.
pub(super) fn render_ladder(
    channel: &Channel<SignalingNetworkEvent>,
    recorder: &Recorder,
) -> Vec<String> {
    let entries = to_sip_entries(&channel.snapshot());
    let names: BTreeMap<SocketAddr, String> = recorder
        .snapshot()
        .lanes
        .into_iter()
        .map(|l| (l.addr, l.names.first().cloned().unwrap_or_default()))
        .collect();
    let label_for = |addr: &SocketAddr| match names.get(addr) {
        Some(n) if !n.is_empty() => format!("{n} ({addr})"),
        _ => addr.to_string(),
    };
    let base = entries.iter().map(|e| e.sent_ms as i64).min().unwrap_or(0);
    if entries.is_empty() {
        return vec!["(no messages recorded)".to_string()];
    }
    entries
        .iter()
        .map(|e| {
            let sent = format_relative(e.sent_ms as i64 - base);
            let ts = match e.received_ms {
                Some(r) if r != e.sent_ms => {
                    format!("{sent} → {}", format_relative(r as i64 - base))
                }
                _ => sent,
            };
            let undelivered = if e.delivered { "" } else { "  [UNDELIVERED]" };
            format!(
                "[{ts}] {} → {}  {}{}",
                label_for(&e.from),
                label_for(&e.to),
                facets(&e.raw).label,
                undelivered
            )
        })
        .collect()
}
