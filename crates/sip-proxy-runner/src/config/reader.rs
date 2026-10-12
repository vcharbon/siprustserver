//! [`EnvReader`] — reads one boot variable at a time through a lookup and
//! collects every problem it meets, so one boot attempt names them all.

use std::net::SocketAddr;

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

impl Span {
    fn holds(self, v: f64) -> bool {
        v.is_finite()
            && match self {
                Span::Fraction => v > 0.0 && v <= 1.0,
                Span::OpenUnit => v > 0.0 && v < 1.0,
                Span::FromZero => (0.0..1.0).contains(&v),
                Span::Positive => v > 0.0,
            }
    }

    fn describe(self) -> &'static str {
        match self {
            Span::Fraction => "a number in (0, 1]",
            Span::OpenUnit => "a number in (0, 1)",
            Span::FromZero => "a number in [0, 1)",
            Span::Positive => "a finite number above 0",
        }
    }
}

/// The variables of one boot, read through `get` (`None` = unset). Unset
/// takes the default; a value set empty, malformed or out of its range is
/// refused.
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

    /// `key` trimmed, `None` when unset; set but empty is refused.
    pub(super) fn present(&mut self, key: &str) -> Option<String> {
        let value = self.raw(key)?;
        let value = value.trim();
        if value.is_empty() {
            self.refuse(format!("{key} is set but empty"));
            return None;
        }
        Some(value.to_string())
    }

    /// `key` as an integer in `min..=max`, `default` when unset.
    pub(super) fn int(&mut self, key: &str, default: u64, min: u64, max: u64) -> u64 {
        let Some(value) = self.present(key) else { return default };
        match value.parse::<u64>() {
            Ok(n) if (min..=max).contains(&n) => n,
            _ => {
                self.refuse(format!("{key} must be an integer from {min} to {max}, got {value:?}"));
                default
            }
        }
    }

    /// `key` as a number within `span`, `default` when unset.
    pub(super) fn real(&mut self, key: &str, default: f64, span: Span) -> f64 {
        let Some(value) = self.present(key) else { return default };
        match value.parse::<f64>() {
            Ok(v) if span.holds(v) => v,
            _ => {
                self.refuse(format!("{key} must be {}, got {value:?}", span.describe()));
                default
            }
        }
    }

    /// `key` as `1|true|yes|on` or `0|false|no|off`, `default` when unset.
    pub(super) fn flag(&mut self, key: &str, default: bool) -> bool {
        let Some(value) = self.present(key) else { return default };
        match value.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            _ => {
                self.refuse(format!("{key} must be 1|true|yes|on|0|false|no|off, got {value:?}"));
                default
            }
        }
    }

    /// `key` as text, `default` when unset.
    pub(super) fn text(&mut self, key: &str, default: &str) -> String {
        self.present(key).unwrap_or_else(|| default.to_string())
    }

    /// `key` as a local bind address, an IP literal with its port (a name is
    /// never looked up at boot); `default` when unset.
    pub(super) fn bind_addr(&mut self, key: &str, default: SocketAddr) -> SocketAddr {
        let Some(value) = self.present(key) else { return default };
        value.parse().unwrap_or_else(|_| {
            self.refuse(format!("{key} must be an IP literal with its port, got {value:?}"));
            default
        })
    }
}
