//! [`HealthProbe`] — OPTIONS keepalive toward the B2BUA workers (port of
//! `health/HealthProbe.ts`). Per tick: fan out an OPTIONS to every registered
//! worker, drain replies for `timeout_ms`, then reap the silent ones. A 200 →
//! `Alive`; a `503 + Reason: …not-ready…` → `NotReady`, any other 503 →
//! `Draining`; `threshold` consecutive misses → `Dead`. Each reply's
//! `X-Overload` payload feeds the [`WorkerLoadObserver`]. Health is written
//! through the [`WorkerRegistryControl`] seam.
//!
//! Scheduling rides `tokio::time` directly (per the sip-clock ADR): under a
//! paused runtime a test advances `interval_ms + timeout_ms (+ε)` per tick.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use sip_clock::Clock;
use sip_message::message_helpers::get_header;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};
use sip_net::UdpEndpoint;
use sip_txn::IdGen;

use crate::load_observer::{parse_x_overload_header, WorkerLoadObserver};
use crate::registry::control::WorkerRegistryControl;
use crate::registry::{WorkerHealth, WorkerRegistry};

/// Probe cadence (source defaults: 1 s interval, 1.5 s timeout, 2 misses → dead).
#[derive(Debug, Clone, Copy)]
pub struct HealthProbeConfig {
    pub interval_ms: u64,
    pub timeout_ms: u64,
    pub threshold: u32,
}

impl Default for HealthProbeConfig {
    fn default() -> Self {
        Self { interval_ms: 1_000, timeout_ms: 1_500, threshold: 2 }
    }
}

/// The OPTIONS keepalive probe. Owns a UAC endpoint on the signaling fabric.
pub struct HealthProbe {
    endpoint: Box<dyn UdpEndpoint>,
    registry: Arc<dyn WorkerRegistry>,
    control: Arc<dyn WorkerRegistryControl>,
    observer: Arc<WorkerLoadObserver>,
    clock: Clock,
    id_gen: Arc<IdGen>,
    config: HealthProbeConfig,
    parser: CustomParser,
}

impl HealthProbe {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        endpoint: Box<dyn UdpEndpoint>,
        registry: Arc<dyn WorkerRegistry>,
        control: Arc<dyn WorkerRegistryControl>,
        observer: Arc<WorkerLoadObserver>,
        clock: Clock,
        id_gen: Arc<IdGen>,
        config: HealthProbeConfig,
    ) -> Self {
        Self { endpoint, registry, control, observer, clock, id_gen, config, parser: CustomParser::default() }
    }

    fn now_ms(&self) -> u64 {
        self.clock.now_ms().max(0) as u64
    }

    /// Run the probe loop forever (spawn it; abort to stop).
    pub async fn run(self) {
        let probe = self.endpoint.local_addr();
        let probe_ip = probe.ip().to_string();
        let probe_port = probe.port();
        let mut misses: HashMap<String, u32> = HashMap::new();

        loop {
            tokio::time::sleep(Duration::from_millis(self.config.interval_ms)).await;

            // ── Fan out OPTIONS to every worker ──────────────────────────
            let now = self.now_ms();
            let mut issued: Vec<(String, String)> = Vec::new(); // (worker_id, call_id)
            let mut pending: HashSet<String> = HashSet::new();
            for w in self.registry.snapshot() {
                let call_id = format!("probe-{}-{}-{}@{}", w.id, now, self.id_gen.new_tag(), probe_ip);
                let req = build_options(&w.address.host, w.address.port, &probe_ip, probe_port, &call_id, &self.id_gen.new_branch());
                if let Some(dst) = w.address.to_socket_addr() {
                    let _ = self.endpoint.send_to(req.as_bytes(), dst).await;
                }
                pending.insert(call_id.clone());
                issued.push((w.id.clone(), call_id));
            }

            // ── Drain replies for the timeout window ─────────────────────
            let deadline = tokio::time::sleep(Duration::from_millis(self.config.timeout_ms));
            tokio::pin!(deadline);
            loop {
                tokio::select! {
                    _ = &mut deadline => break,
                    maybe = self.endpoint.recv() => {
                        match maybe {
                            Some(pkt) => self.handle_reply(&pkt.raw, &mut pending, &mut misses),
                            None => return, // endpoint closed
                        }
                    }
                }
            }

            // ── Reap silent probes ───────────────────────────────────────
            for (id, call_id) in &issued {
                if !pending.contains(call_id) {
                    continue; // a reply arrived
                }
                let n = misses.entry(id.clone()).or_insert(0);
                *n += 1;
                if *n >= self.config.threshold {
                    self.control.set_health(id, WorkerHealth::Dead);
                }
            }
        }
    }

    fn handle_reply(&self, raw: &[u8], pending: &mut HashSet<String>, misses: &mut HashMap<String, u32>) {
        let Ok(SipMessage::Response(resp)) = self.parser.parse(raw) else {
            return;
        };
        if resp.call_id.is_empty() {
            return;
        }
        // Identify the worker: fast path via the pending set, else recover from
        // the probe Call-ID prefix WE minted and confirm it is registered.
        let id = if pending.remove(&resp.call_id) {
            worker_id_from_call_id(&resp.call_id)
        } else {
            match parse_probe_call_id(&resp.call_id) {
                Some(id) if self.registry.resolve(&id).is_some() => Some(id),
                _ => None,
            }
        };
        let Some(id) = id else { return };

        // Any reply resets the miss counter.
        misses.insert(id.clone(), 0);

        let reason = get_header(&resp.headers, "reason");
        let health = match resp.status {
            200 => WorkerHealth::Alive,
            503 => classify_503(reason),
            _ => WorkerHealth::Alive,
        };
        self.control.set_health(&id, health);

        // Feed the AIMD observer with the self-reported overload payload.
        if let Some(payload) = parse_x_overload_header(get_header(&resp.headers, "x-overload")) {
            self.observer.apply_payload(&id, &payload, self.now_ms());
        }
    }
}

fn build_options(host: &str, port: u16, probe_ip: &str, probe_port: u16, call_id: &str, branch: &str) -> String {
    format!(
        "OPTIONS sip:{host}:{port} SIP/2.0\r\n\
Via: SIP/2.0/UDP {probe_ip}:{probe_port};branch={branch}\r\n\
From: <sip:probe@{probe_ip}>;tag=probe\r\n\
To: <sip:probe@{host}:{port}>\r\n\
Call-ID: {call_id}\r\n\
CSeq: 1 OPTIONS\r\n\
Max-Forwards: 70\r\n\
Contact: <sip:probe@{probe_ip}:{probe_port}>\r\n\
Content-Length: 0\r\n\r\n"
    )
}

fn classify_503(reason: Option<&str>) -> WorkerHealth {
    match reason {
        Some(r) if r.to_ascii_lowercase().contains("not-ready") => WorkerHealth::NotReady,
        _ => WorkerHealth::Draining,
    }
}

/// The worker id is the segment between `probe-` and the trailing
/// `-<nowMs>-<tag>@…` we minted. Returns `None` for any non-minted shape.
fn parse_probe_call_id(call_id: &str) -> Option<String> {
    let rest = call_id.strip_prefix("probe-")?;
    let (before_at, _) = rest.split_once('@')?;
    // `<id>-<nowMs>-<tag>` — split the last two `-` fields off; the remainder
    // (which may itself contain `-`, e.g. `b2bua-worker-0`) is the id.
    let mut parts = before_at.rsplitn(3, '-');
    let _tag = parts.next()?;
    let now = parts.next()?;
    let id = parts.next()?;
    if id.is_empty() || now.is_empty() || !now.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(id.to_string())
}

/// Fast-path id recovery for a Call-ID we know we minted (already validated by
/// the pending set).
fn worker_id_from_call_id(call_id: &str) -> Option<String> {
    parse_probe_call_id(call_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minted_probe_call_id_with_dashes_in_id() {
        assert_eq!(parse_probe_call_id("probe-b2bua-worker-0-1234567-ab12@10.0.0.1"), Some("b2bua-worker-0".to_string()));
        assert_eq!(parse_probe_call_id("probe-w1-42-tag@host"), Some("w1".to_string()));
    }

    #[test]
    fn rejects_non_minted_call_ids() {
        assert!(parse_probe_call_id("not-a-probe@h").is_none());
        assert!(parse_probe_call_id("probe-w1@h").is_none());
        assert!(parse_probe_call_id("probe-w1-notanumber-tag@h").is_none());
    }

    #[test]
    fn classify_503_distinguishes_not_ready() {
        assert_eq!(classify_503(Some("SIP;cause=503;text=\"not-ready (boot drain)\"")), WorkerHealth::NotReady);
        assert_eq!(classify_503(Some("SIP;cause=503;text=\"draining\"")), WorkerHealth::Draining);
        assert_eq!(classify_503(None), WorkerHealth::Draining);
    }
}
