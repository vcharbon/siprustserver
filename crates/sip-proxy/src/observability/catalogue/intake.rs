//! The families of the datagrams the receive path drops at or above the
//! intake watermark, before they are queued.

use metric_catalogue::{Family, Labels};

pub const INTAKE_SHED: Family = Family::counter(
    "sip_proxy_intake_shed_total",
    Labels::None,
    "New non-emergency INVITEs dropped by the depth-watermark pre-ingress shed (the selective last-line guard below the admission layer).",
);

pub const INTAKE_UNANSWERABLE: Family = Family::counter(
    "sip_proxy_intake_unanswerable_total",
    Labels::None,
    "Datagrams dropped at or above the intake watermark because no response can answer them: refused by the SIP parser with nothing a 400 could echo, or a parser panic. Each is also counted in sip_proxy_parse_errors_total or sip_proxy_parse_panics_total.",
);
