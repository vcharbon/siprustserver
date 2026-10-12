//! [`EnvReader`] — reads one boot variable at a time through a lookup and
//! collects every problem it meets, so one boot attempt names them all.

use std::net::{SocketAddr, ToSocketAddrs};

/// Where a real-valued variable must fall.
#[derive(Debug, Clone, Copy)]
pub(super) enum Span {
    /// (0, 1]
    Fraction,
    /// (0, 1)
    OpenUnit,
    /// [0, 1)
    FromZero,
    /// (0, ∞)
    Positive,
}

/// The variables of one boot, read through `get` (`None` = unset).
pub(super) struct EnvReader<'a> {
    get: &'a dyn Fn(&str) -> Option<String>,
    errors: Vec<String>,
}

impl<'a> EnvReader<'a> {
    pub(super) fn new(get: &'a dyn Fn(&str) -> Option<String>) -> Self {
        Self { get, errors: Vec::new() }
    }

    /// The problems met so far.
    pub(super) fn into_errors(self) -> Vec<String> {
        self.errors
    }

    pub(super) fn refuse(&mut self, problem: String) {
        self.errors.push(problem);
    }

    /// `key` as given, `None` when unset.
    pub(super) fn raw(&self, key: &str) -> Option<String> {
        (self.get)(key)
    }

    /// `key` as an integer in `min..=max`, `default` when unset.
    pub(super) fn int(&mut self, key: &str, default: u64, _min: u64, _max: u64) -> u64 {
        self.raw(key).and_then(|v| v.trim().parse().ok()).unwrap_or(default)
    }

    /// `key` as a number within `span`, `default` when unset.
    pub(super) fn real(&mut self, key: &str, default: f64, _span: Span) -> f64 {
        self.raw(key)
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|v| v.is_finite())
            .unwrap_or(default)
    }

    /// `key` as `1|true|yes|on` or `0|false|no|off`, `default` when unset.
    pub(super) fn flag(&mut self, key: &str, default: bool) -> bool {
        match self.raw(key).map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            Some("1" | "true" | "yes" | "on") => true,
            Some("0" | "false" | "no" | "off") => false,
            _ => default,
        }
    }

    /// `key` as text, `default` when unset.
    pub(super) fn text(&mut self, key: &str, default: &str) -> String {
        self.raw(key).unwrap_or_else(|| default.to_string())
    }

    /// `key` as a local bind address, `default` when unset.
    pub(super) fn bind_addr(&mut self, key: &str, default: SocketAddr) -> SocketAddr {
        let Some(value) = self.raw(key) else { return default };
        match value.to_socket_addrs().ok().and_then(|mut a| a.next()) {
            Some(addr) => addr,
            None => {
                self.refuse(format!("{key} cannot be resolved: {value:?}"));
                default
            }
        }
    }
}
