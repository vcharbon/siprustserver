//! B2BUA runtime configuration — the subset of the source `AppConfig` the
//! dispatcher / router / store / rules read. Behavioural timeouts that have a
//! tokio analogue stay here as plain values.

/// Tunables for a B2BUA worker. Cheap to clone (a handful of scalars + two
/// short strings); share one instance across the stack.
#[derive(Clone, Debug)]
pub struct B2buaConfig {
    /// This worker's ordinal, encoded into `callRef` for partition routing.
    pub self_ordinal: String,
    /// Local signaling IP stamped into Via / Contact.
    pub sip_local_ip: String,
    /// Local signaling port stamped into Via / Contact.
    pub sip_local_port: u16,
    /// Global cap on concurrently-running handlers across all calls.
    pub event_dispatch_concurrency: usize,
    /// Per-call queue depth (events buffered behind a busy handler).
    pub per_call_queue_depth: usize,
    /// Max number of live per-call queues (memory bound).
    pub per_call_queue_cap: usize,
    /// Auto-terminate a call after this many processed messages (loop guard).
    pub max_messages_per_call: u64,
    /// Bounded CDR submit queue; `0` disables buffering (passthrough).
    pub cdr_buffer_queue_max: usize,
}

impl Default for B2buaConfig {
    fn default() -> Self {
        Self {
            self_ordinal: "w0".to_string(),
            sip_local_ip: "127.0.0.1".to_string(),
            sip_local_port: 5060,
            event_dispatch_concurrency: 1024,
            per_call_queue_depth: 64,
            per_call_queue_cap: 200_000,
            max_messages_per_call: 5_000,
            cdr_buffer_queue_max: 1_024,
        }
    }
}
