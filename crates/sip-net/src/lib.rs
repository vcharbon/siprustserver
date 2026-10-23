//! sip-net — the SIP signaling network layer.
//!
//! The DI seam is the [`SignalingNetwork`] trait ([`net`]); its production
//! implementation is [`RealSignalingNetwork`], `tokio::net::UdpSocket`-backed
//! ([`real`]). The simulated fabric, the recording and audit decorators and the
//! wire-trace projection implement the same seam in `sip-net-testkit`. A
//! datagram's parse made on the receive path ([`Arrival`]) travels with it
//! through the queue, so it is parsed once.

// The panic set at deny outside test code (ADR-0051 §3).
#![cfg_attr(
    not(test),
    deny(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::string_slice,
        clippy::todo,
        clippy::unimplemented,
        clippy::unreachable,
        clippy::unwrap_used
    )
)]

pub mod arrival;
pub mod fragmentation;
pub mod net;
pub mod parse_guard;
pub mod pre_ingress;
pub mod queue;
pub mod real;
pub mod socket_stats;
pub mod types;

pub use arrival::Arrival;
pub use net::{SignalingNetwork, UdpEndpoint};
pub use parse_guard::{parse_datagram, ParseOutcome, ParserPanicked};
pub use real::RealSignalingNetwork;
pub use sip_message::ip_literal::{ip_host, ip_socket_addr};
pub use types::{
    all_ua_roles, BindError, BindErrorReason, BindSummary, BindUdpOpts, PreIngressAction,
    PreIngressHook, ReEmitKind, RecvDisposition, RecvTap, SendError, SendErrorKind, SendTap,
    SocketBuffers, UaRole, UdpEndpointCounters, UdpPacket, UndeliveredPacket, MAX_UDP_PAYLOAD,
};
