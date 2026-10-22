//! The families of the datagrams the SIP parser refused or panicked on.

use metric_catalogue::{Family, Labels};

pub const PARSE_PANICS: Family = Family::counter(
    "sip_proxy_parse_panics_total",
    Labels::None,
    "Inbound datagrams the SIP parser panicked on, dropped at intake or by the recv shard; each one is a parser defect, expected 0.",
);

pub const PARSE_ERRORS: Family = Family::counter(
    "sip_proxy_parse_errors_total",
    Labels::None,
    "Inbound datagrams the SIP parser refused, dropped unanswered at intake or by the recv shard (a response, an ACK, a request no response can echo, a limit): a malformed-traffic flood or a parser regression.",
);

pub const PARSE_REFUSALS_ANSWERED: Family = Family::counter(
    "sip_proxy_parse_refusals_answered_total",
    Labels::None,
    "Inbound requests the SIP parser refused and the proxy answered with a stateless 400 (RFC 3261 section 16.3).",
);

pub const PARSE_REFUSALS_CAPPED: Family = Family::counter(
    "sip_proxy_parse_refusals_capped_total",
    Labels::None,
    "Inbound requests the SIP parser refused that a 400 could answer, dropped because the token bucket capping those 400s was empty.",
);
