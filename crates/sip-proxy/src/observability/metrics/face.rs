//! The proxy's faces in its metrics: the face a datagram crossed, the traffic
//! per face, and the claims of the internal plane the external face does not
//! honour (ADR-0048).

use std::sync::atomic::Ordering;

use super::{Direction, ProxyMetrics};

/// Which proxy face a datagram crossed, for
/// `sip_proxy_face_messages_total{label="face:direction"}` (dual-face mode). A
/// single-face proxy records everything as `int`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Face {
    Internal,
    External,
}

impl Face {
    /// Every face, in declaration order.
    pub const ALL: [Face; 2] = [Face::Internal, Face::External];

    /// The face's metrics label, and the value a trace event names it by.
    pub const fn as_str(self) -> &'static str {
        match self {
            Face::Internal => "int",
            Face::External => "ext",
        }
    }
}

/// A claim of the internal plane met on the external face, for
/// `sip_proxy_external_untrusted_total{kind}`. One increment is one message
/// carrying that claim; the message itself is still handled as an inbound one,
/// except a response, which is dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UntrustedClaim {
    /// The proxy's own Route entry carries the `;outbound` marker: ignored.
    OutboundMarker,
    /// The top Via sent-by is a registry worker: ignored.
    WorkerVia,
    /// The datagram source is a registry worker: ignored.
    WorkerSource,
    /// A Route entry follows the proxy's own: every such entry is removed.
    ForeignRoute,
    /// A response's next Via is no registry worker: the response is dropped.
    ResponseNotToWorker,
}

impl UntrustedClaim {
    /// Every claim, in declaration order.
    pub const ALL: [UntrustedClaim; 5] = [
        UntrustedClaim::OutboundMarker,
        UntrustedClaim::WorkerVia,
        UntrustedClaim::WorkerSource,
        UntrustedClaim::ForeignRoute,
        UntrustedClaim::ResponseNotToWorker,
    ];

    /// The `kind` label.
    pub const fn as_str(self) -> &'static str {
        match self {
            UntrustedClaim::OutboundMarker => "outbound_marker",
            UntrustedClaim::WorkerVia => "worker_via",
            UntrustedClaim::WorkerSource => "worker_source",
            UntrustedClaim::ForeignRoute => "foreign_route",
            UntrustedClaim::ResponseNotToWorker => "response_not_to_worker",
        }
    }
}

impl ProxyMetrics {
    /// Count one datagram RECEIVED on `face` (the recv loop's arrival socket).
    pub fn record_face_ingress(&self, face: Face) {
        self.face_messages[face as usize][Direction::Inbound as usize]
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Count one datagram EGRESSED on `face` (the destination-picked socket).
    pub fn record_face_egress(&self, face: Face) {
        self.face_messages[face as usize][Direction::Outbound as usize]
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Read one per-face counter (test/assertion surface).
    pub fn face_messages_total(&self, face: Face, direction: Direction) -> u64 {
        self.face_messages[face as usize][direction as usize].load(Ordering::Relaxed)
    }

    /// Count one message on the external face carrying `claim`.
    pub fn record_external_untrusted(&self, claim: UntrustedClaim) {
        self.external_untrusted.add(&[claim.as_str()], 1);
    }

    /// Read one `sip_proxy_external_untrusted_total` series.
    pub fn external_untrusted_total(&self, claim: UntrustedClaim) -> u64 {
        self.external_untrusted.get(&[claim.as_str()])
    }
}
