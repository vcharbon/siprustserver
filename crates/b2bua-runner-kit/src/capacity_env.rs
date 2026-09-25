//! The capacity ceilings' env grammar (ADR-0037). Every ceiling is explicit:
//! nothing is derived from the host or the container.
//!
//! | variable | ceiling |
//! |---|---|
//! | `B2BUA_MAX_CALLS` / `B2BUA_MAX_CALLS_EMERGENCY` | live calls |
//! | `B2BUA_MAX_TXNS` / `B2BUA_MAX_TXNS_EMERGENCY` | live transactions |
//! | `B2BUA_MAX_RSS` / `B2BUA_MAX_RSS_EMERGENCY` | process RSS, bytes |
//! | `B2BUA_REPL_BACKUP_MAX_CALLS` | backup replicas held |
//! | `B2BUA_REPL_BACKUP_MAX_RSS` | process RSS for a new backup, bytes |
//!
//! A byte value is an integer with an optional `Ki`/`Mi`/`Gi` suffix. Unset or
//! blank leaves that ceiling off.

use b2bua::config::{CapacityConfig, Ceilings};

use crate::stated;

/// The ceilings `lookup` states. An unparsable value is an `Err` naming it.
pub(crate) fn capacity_from_lookup(
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<CapacityConfig, String> {
    let count = |key: &str| -> Result<Option<u64>, String> {
        stated(lookup(key))
            .map(|v| v.parse::<u64>().map_err(|e| format!("{key}={v:?}: {e}")))
            .transpose()
    };
    let bytes = |key: &str| -> Result<Option<u64>, String> {
        stated(lookup(key))
            .map(|v| parse_bytes(&v).map_err(|e| format!("{key}={v:?}: {e}")))
            .transpose()
    };
    Ok(CapacityConfig {
        calls: Ceilings {
            normal: count("B2BUA_MAX_CALLS")?,
            emergency: count("B2BUA_MAX_CALLS_EMERGENCY")?,
        },
        transactions: Ceilings {
            normal: count("B2BUA_MAX_TXNS")?,
            emergency: count("B2BUA_MAX_TXNS_EMERGENCY")?,
        },
        rss_bytes: Ceilings {
            normal: bytes("B2BUA_MAX_RSS")?,
            emergency: bytes("B2BUA_MAX_RSS_EMERGENCY")?,
        },
        backup_calls: count("B2BUA_REPL_BACKUP_MAX_CALLS")?,
        backup_rss_bytes: bytes("B2BUA_REPL_BACKUP_MAX_RSS")?,
    })
}

/// `1536Mi` → 1536·2²⁰. Plain integers are bytes.
fn parse_bytes(v: &str) -> Result<u64, String> {
    let v = v.trim();
    let (digits, shift) = match v {
        _ if v.ends_with("Ki") => (&v[..v.len() - 2], 10),
        _ if v.ends_with("Mi") => (&v[..v.len() - 2], 20),
        _ if v.ends_with("Gi") => (&v[..v.len() - 2], 30),
        _ => (v, 0),
    };
    let n: u64 = digits.trim().parse().map_err(|e| format!("{e}"))?;
    n.checked_mul(1u64 << shift).ok_or_else(|| "overflows u64".to_string())
}

#[cfg(test)]
mod capacity_env_tests {
    use super::*;

    fn lookup(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
    }

    #[test]
    fn nothing_stated_leaves_every_ceiling_off() {
        assert_eq!(capacity_from_lookup(lookup(&[])), Ok(CapacityConfig::default()));
    }

    #[test]
    fn every_ceiling_is_read() {
        let cfg = capacity_from_lookup(lookup(&[
            ("B2BUA_MAX_CALLS", "8000"),
            ("B2BUA_MAX_CALLS_EMERGENCY", "9000"),
            ("B2BUA_MAX_TXNS", "40000"),
            ("B2BUA_MAX_TXNS_EMERGENCY", "45000"),
            ("B2BUA_MAX_RSS", "1536Mi"),
            ("B2BUA_MAX_RSS_EMERGENCY", "2Gi"),
            ("B2BUA_REPL_BACKUP_MAX_CALLS", "8000"),
            ("B2BUA_REPL_BACKUP_MAX_RSS", "1048576"),
        ]))
        .expect("parses");
        assert_eq!(cfg.calls, Ceilings { normal: Some(8000), emergency: Some(9000) });
        assert_eq!(cfg.transactions, Ceilings { normal: Some(40000), emergency: Some(45000) });
        assert_eq!(cfg.rss_bytes, Ceilings { normal: Some(1536 << 20), emergency: Some(2 << 30) });
        assert_eq!(cfg.backup_calls, Some(8000));
        assert_eq!(cfg.backup_rss_bytes, Some(1 << 20));
    }

    #[test]
    fn a_blank_value_is_unset() {
        let cfg = capacity_from_lookup(lookup(&[("B2BUA_MAX_CALLS", "  ")])).expect("parses");
        assert_eq!(cfg.calls.normal, None);
    }

    #[test]
    fn an_unparsable_value_names_its_variable() {
        let err = capacity_from_lookup(lookup(&[("B2BUA_MAX_RSS", "2GB")])).unwrap_err();
        assert!(err.starts_with("B2BUA_MAX_RSS="), "{err}");
        let err = capacity_from_lookup(lookup(&[("B2BUA_MAX_CALLS", "-1")])).unwrap_err();
        assert!(err.starts_with("B2BUA_MAX_CALLS="), "{err}");
    }

    #[test]
    fn bytes_take_binary_suffixes() {
        assert_eq!(parse_bytes("4096"), Ok(4096));
        assert_eq!(parse_bytes("4Ki"), Ok(4096));
        assert_eq!(parse_bytes("3Mi"), Ok(3 << 20));
        assert!(parse_bytes("99999999999Gi").is_err());
    }
}
