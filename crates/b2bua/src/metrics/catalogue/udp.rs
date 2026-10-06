//! The UDP transport's families: the inbound queue and the socket's drops.

use metric_catalogue::{Family, Labels};

pub const QUEUE_DEPTH: Family = Family::gauge(
    "b2bua_udp_queue_depth",
    Labels::None,
    "Live inbound UDP queue depth (port of UdpTransportMetrics.queueDepth).",
);

pub const QUEUE_MAX: Family = Family::gauge(
    "b2bua_udp_queue_max",
    Labels::None,
    "Configured inbound UDP queue bound (udpQueueMax).",
);

pub const TAIL_DROPPED: Family = Family::counter(
    "b2bua_udp_tail_dropped_total",
    Labels::None,
    "Datagrams tail-dropped by the full inbound queue (port of UdpTransportMetrics.dropsTailDrop).",
);

pub const SEND_WOULD_BLOCK: Family = Family::counter(
    "b2bua_udp_send_would_block_total",
    Labels::None,
    "Outbound datagrams dropped because the socket's send buffer was full (a blocking send would have parked the transaction owner; ADR-0033).",
);

pub const KERNEL_RX_DROPPED: Family = Family::counter(
    "b2bua_udp_kernel_rx_dropped_total",
    Labels::None,
    "Inbound datagrams the kernel dropped on the signalling socket before the stack read them, mostly on a full receive buffer (SO_RCVBUF, B2BUA_UDP_RCVBUF).",
);
