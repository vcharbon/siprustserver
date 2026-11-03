//! [`GuardConfig`] — every variable the guard reads at start, read once
//! through a lookup. Unset takes the default; a value set empty, malformed or
//! out of range refuses the start, naming every problem.
//!
//! | variable | default | role |
//! |---|---|---|
//! | `ROUTE_GUARD_FITNESS` | `127.0.0.1:9090` | the proxy's probe server, `ip:port`; `/fitz` is read there |
//! | `ROUTE_GUARD_PEERS` | required | the node's BGP routers, comma-separated IPv4 addresses |
//! | `ROUTE_GUARD_POLL_MS` | 100 | one fitness reading per period, each bounded by it, 10..=10000 |
//! | `ROUTE_GUARD_DEAD_MS` | 1000 | how long one refresh lets the node out; at least 4 × the poll |
//! | `ROUTE_GUARD_HOLD_MS` | 3000 | how long the proxy reads fit before a withdrawn node is let out, 0..=600000 |
//! | `ROUTE_GUARD_METRICS` | `0.0.0.0:9095` | the guard's `/metrics` and `/healthz`, `ip:port` |
//! | `ROUTE_GUARD_NFT` | `nft` | the nftables command |

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use crate::switch::Timing;

/// The guard's start configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardConfig {
    pub fitness: SocketAddr,
    pub peers: Vec<Ipv4Addr>,
    pub poll: Duration,
    pub timing: Timing,
    pub metrics: SocketAddr,
    pub nft: String,
}

/// The dead bound holds this many polls, so a fit proxy is refreshed several
/// times within each bound.
pub const POLLS_PER_DEAD_BOUND: u64 = 4;

impl GuardConfig {
    /// Reads every variable through `get` (`None` = unset). `Err` holds every
    /// problem, one line each.
    pub fn from_lookup(get: &dyn Fn(&str) -> Option<String>) -> Result<Self, Vec<String>> {
        let mut errors = Vec::new();
        let mut addr = |key: &str, default: &str| {
            let raw = get(key).unwrap_or_else(|| default.to_string());
            raw.trim().parse::<SocketAddr>().unwrap_or_else(|_| {
                errors.push(format!("{key} must be an ip:port, got {raw:?}"));
                SocketAddr::from(([127, 0, 0, 1], 0))
            })
        };
        let fitness = addr("ROUTE_GUARD_FITNESS", "127.0.0.1:9090");
        let metrics = addr("ROUTE_GUARD_METRICS", "0.0.0.0:9095");
        let mut ms = |key: &str, default: u64, min: u64, max: u64| match get(key) {
            None => default,
            Some(raw) => match raw.trim().parse::<u64>() {
                Ok(v) if (min..=max).contains(&v) => v,
                _ => {
                    errors.push(format!("{key} must be an integer in {min}..={max}, got {raw:?}"));
                    default
                }
            },
        };
        let poll_ms = ms("ROUTE_GUARD_POLL_MS", 100, 10, 10_000);
        let dead_ms = ms("ROUTE_GUARD_DEAD_MS", 1_000, 40, 600_000);
        let hold_ms = ms("ROUTE_GUARD_HOLD_MS", 3_000, 0, 600_000);
        let peers = peers(get("ROUTE_GUARD_PEERS"), &mut errors);
        let nft = get("ROUTE_GUARD_NFT").unwrap_or_else(|| "nft".to_string());
        if nft.trim().is_empty() {
            errors
                .push("ROUTE_GUARD_NFT must name the nftables command, got an empty value".into());
        }
        if dead_ms < POLLS_PER_DEAD_BOUND.saturating_mul(poll_ms) {
            errors.push(format!(
                "ROUTE_GUARD_DEAD_MS={dead_ms} must be at least {POLLS_PER_DEAD_BOUND} × \
                 ROUTE_GUARD_POLL_MS={poll_ms}: a fit proxy's refresh would run out between \
                 two readings"
            ));
        }
        if !errors.is_empty() {
            return Err(errors);
        }
        Ok(Self {
            fitness,
            peers,
            poll: Duration::from_millis(poll_ms),
            timing: Timing {
                dead: Duration::from_millis(dead_ms),
                hold: Duration::from_millis(hold_ms),
            },
            metrics,
            nft: nft.trim().to_string(),
        })
    }

    /// Reads the process environment.
    pub fn from_env() -> Result<Self, Vec<String>> {
        Self::from_lookup(&|key| std::env::var(key).ok())
    }
}

/// `ROUTE_GUARD_PEERS`: one IPv4 address at least, each once.
fn peers(raw: Option<String>, errors: &mut Vec<String>) -> Vec<Ipv4Addr> {
    let Some(raw) = raw else {
        errors
            .push("ROUTE_GUARD_PEERS is required: the routers the node's BFD and BGP reach".into());
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        match entry.parse::<Ipv4Addr>() {
            Ok(ip) if !out.contains(&ip) => out.push(ip),
            Ok(ip) => errors.push(format!("ROUTE_GUARD_PEERS names {ip} twice")),
            Err(_) => {
                errors.push(format!("ROUTE_GUARD_PEERS entry {entry:?} is not an IPv4 address"))
            }
        }
    }
    if out.is_empty() && !errors.iter().any(|e| e.starts_with("ROUTE_GUARD_PEERS")) {
        errors.push(format!("ROUTE_GUARD_PEERS names no router, got {raw:?}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(pairs: &[(&str, &str)]) -> Result<GuardConfig, Vec<String>> {
        let map: std::collections::HashMap<String, String> =
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        GuardConfig::from_lookup(&move |key| map.get(key).cloned())
    }

    #[test]
    fn the_defaults_with_the_peers_named() {
        let cfg = read(&[("ROUTE_GUARD_PEERS", "192.0.2.1, 198.51.100.1")]).expect("valid");
        assert_eq!(cfg.peers, vec![Ipv4Addr::new(192, 0, 2, 1), Ipv4Addr::new(198, 51, 100, 1)]);
        assert_eq!(cfg.fitness, "127.0.0.1:9090".parse().unwrap());
        assert_eq!(cfg.poll, Duration::from_millis(100));
        assert_eq!(
            cfg.timing,
            Timing { dead: Duration::from_millis(1_000), hold: Duration::from_millis(3_000) }
        );
        assert_eq!(cfg.nft, "nft");
    }

    #[test]
    fn every_problem_refuses_the_start_at_once() {
        let errors = read(&[
            ("ROUTE_GUARD_PEERS", "192.0.2.1,2001:db8::1"),
            ("ROUTE_GUARD_POLL_MS", "300"),
            ("ROUTE_GUARD_FITNESS", "localhost:9090"),
        ])
        .expect_err("refused");
        assert_eq!(errors.len(), 3, "{errors:?}");
        assert!(errors.iter().any(|e| e.contains("\"2001:db8::1\"")), "{errors:?}");
        assert!(errors.iter().any(|e| e.contains("ROUTE_GUARD_FITNESS")), "{errors:?}");
        assert!(
            errors
                .iter()
                .any(|e| e.contains("ROUTE_GUARD_DEAD_MS=1000")
                    && e.contains("ROUTE_GUARD_POLL_MS=300")),
            "{errors:?}"
        );
        assert!(read(&[]).expect_err("no peers")[0].contains("ROUTE_GUARD_PEERS is required"));
    }
}
