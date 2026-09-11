//! The system under test's own address set, and the side it places an
//! endpoint on.
//!
//! A hit's `emitter_role` is what the GROUP TOPOLOGY says (which endpoint the
//! call's legs cross); the side is what a DEPLOYMENT STATEMENT says. The two
//! are kept apart on purpose: a capture where the crossed box is a peer's
//! B2BUA reads `platform` in the role and `peer` on the side, and a consumer
//! that wants to know whether the report disagrees with the topology must be
//! able to see both.
//!
//! Exact IP addresses, port-insensitive: a B2BUA answers on several ports of
//! one address and a peer never shares that address. No CIDR — a subnet is a
//! candidate space, never a statement of which box IS the SUT.

use std::collections::BTreeSet;
use std::net::IpAddr;

use serde::{Deserialize, Serialize};

use crate::flow::{Flows, MatchEvidence};

use super::endpoint_ip;

/// Which side a deployment statement places an endpoint on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Side {
    /// One of the SUT's own addresses.
    Platform,
    /// Any other address, once a SUT set was stated.
    Peer,
    /// No SUT set was stated, so no side is attributable.
    #[default]
    Unattributed,
}

impl Side {
    pub fn token(self) -> &'static str {
        match self {
            Side::Platform => "platform",
            Side::Peer => "peer",
            Side::Unattributed => "unattributed",
        }
    }

    pub fn is_unattributed(&self) -> bool {
        *self == Side::Unattributed
    }
}

/// The addresses that ARE the system under test, and how they were decided.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SutSet {
    /// Sorted, deduplicated, as text.
    pub addresses: Vec<String>,
    /// `stated` (given by the caller) or `mint-point` (derived from the
    /// capture, see [`SutSet::minted_in`]).
    pub decided_by: String,
    #[serde(skip)]
    ips: BTreeSet<IpAddr>,
}

impl SutSet {
    /// A stated set. Every entry must be an IP address; a `host:port` is
    /// refused rather than read as its host, so a typo cannot silently widen
    /// or narrow the platform.
    pub fn stated<S: AsRef<str>>(addresses: &[S]) -> Result<SutSet, String> {
        let mut ips = BTreeSet::new();
        for a in addresses {
            let a = a.as_ref().trim();
            let ip: IpAddr =
                a.parse().map_err(|_| format!("--sut wants an IP address, got {a:?}"))?;
            ips.insert(ip);
        }
        if ips.is_empty() {
            return Err("--sut wants at least one address".into());
        }
        Ok(SutSet::of(ips, "stated"))
    }

    /// The set the capture itself names: every socket that re-originated a
    /// call under a derived Call-ID (`derived_call_id` evidence, the AS side).
    /// `None` when no group carries that evidence — a capture with no
    /// derivation states no platform, and guessing one would be a side
    /// attributed on no ground.
    pub fn minted_in(flows: &Flows) -> Option<SutSet> {
        let ips: BTreeSet<IpAddr> = flows
            .groups
            .iter()
            .flat_map(|g| g.evidence.iter())
            .filter_map(|e| match e {
                MatchEvidence::DerivedCallId { as_socket, .. } => Some(as_socket.ip()),
                _ => None,
            })
            .collect();
        (!ips.is_empty()).then(|| SutSet::of(ips, "mint-point"))
    }

    fn of(ips: BTreeSet<IpAddr>, decided_by: &str) -> SutSet {
        SutSet {
            addresses: ips.iter().map(|ip| ip.to_string()).collect(),
            decided_by: decided_by.to_string(),
            ips,
        }
    }

    /// Whether `endpoint` (`ip:port`, optionally `#label`-suffixed) is one of
    /// the SUT's own addresses.
    pub fn contains(&self, endpoint: &str) -> bool {
        endpoint_ip(endpoint).is_some_and(|ip| self.ips.contains(&ip))
    }

    /// The side this set places `endpoint` on.
    pub fn side_of(&self, endpoint: &str) -> Side {
        if self.contains(endpoint) {
            Side::Platform
        } else {
            Side::Peer
        }
    }

    /// Whether any endpoint of any hop of `legs` is on the platform — the
    /// selection test: a call the SUT never touched is not under review.
    pub fn touches(&self, legs: &[&crate::flow::FlowLeg]) -> bool {
        legs.iter().any(|leg| {
            leg.hops.iter().any(|h| self.ips.contains(&h.a.ip()) || self.ips.contains(&h.b.ip()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stated_set_is_port_insensitive_and_refuses_a_socket() {
        let sut = SutSet::stated(&["10.0.0.2", "10.0.0.2", "2001:db8::1"]).expect("addresses");
        assert_eq!(sut.addresses, vec!["10.0.0.2", "2001:db8::1"]);
        assert_eq!(sut.decided_by, "stated");
        assert_eq!(sut.side_of("10.0.0.2:5060"), Side::Platform);
        assert_eq!(sut.side_of("10.0.0.2:5072#b"), Side::Platform);
        assert_eq!(sut.side_of("[2001:db8::1]:5060"), Side::Platform);
        assert_eq!(sut.side_of("10.0.0.3:5060"), Side::Peer);
        assert!(SutSet::stated(&["10.0.0.2:5060"]).is_err());
        assert!(SutSet::stated::<&str>(&[]).is_err());
    }
}
