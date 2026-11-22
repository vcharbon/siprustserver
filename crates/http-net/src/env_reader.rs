//! [`EnvReader`] — reads one backend's env grammar under its prefix. A value
//! that does not read is kept with the reason and its default stands in, so
//! one refusal at boot names every variable of the set that is wrong.

use std::time::Duration;

use crate::ejection::StatusSet;

/// The values of one prefix, and every one refused so far.
pub(crate) struct EnvReader<L> {
    prefix: String,
    lookup: L,
    refused: Vec<String>,
}

impl<L: Fn(&str) -> Option<String>> EnvReader<L> {
    /// A reader of the variables named `<prefix>_<suffix>`.
    pub(crate) fn new(prefix: &str, lookup: L) -> Self {
        Self { prefix: prefix.to_string(), lookup, refused: Vec::new() }
    }

    /// The variable named `<prefix>_<suffix>`.
    pub(crate) fn key(&self, suffix: &str) -> String {
        format!("{}_{suffix}", self.prefix)
    }

    /// The trimmed value of `<prefix>_<suffix>`; `None` when unset or blank.
    pub(crate) fn stated(&self, suffix: &str) -> Option<String> {
        (self.lookup)(&self.key(suffix)).map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
    }

    /// Refuse the set, `why` naming the variables involved.
    pub(crate) fn refuse(&mut self, why: String) {
        self.refused.push(why);
    }

    /// An integer of at least `min`, `default` when unstated or refused.
    pub(crate) fn at_least(&mut self, suffix: &str, min: u64, default: u64) -> u64 {
        let Some(v) = self.stated(suffix) else { return default };
        let key = self.key(suffix);
        match v.parse::<u64>() {
            Ok(0) if min == 1 => self.refuse(format!("{key}=0: must be positive")),
            Ok(n) if n < min => self.refuse(format!("{key}={n}: must be at least {min}")),
            Ok(n) => return n,
            Err(e) => self.refuse(format!("{key}={v:?}: {e}")),
        }
        default
    }

    /// A positive integer, `default` when unstated or refused.
    pub(crate) fn positive(&mut self, suffix: &str, default: u64) -> u64 {
        self.at_least(suffix, 1, default)
    }

    /// A count of at least `min` this platform holds.
    pub(crate) fn count(&mut self, suffix: &str, min: u64, default: usize) -> usize {
        let n = self.at_least(suffix, min, default as u64);
        usize::try_from(n).unwrap_or_else(|_| {
            let key = self.key(suffix);
            self.refuse(format!("{key}={n}: past this platform"));
            default
        })
    }

    /// A 32-bit count of at least `min`.
    pub(crate) fn count_u32(&mut self, suffix: &str, min: u64, default: u32) -> u32 {
        let n = self.at_least(suffix, min, u64::from(default));
        u32::try_from(n).unwrap_or_else(|_| {
            let key = self.key(suffix);
            self.refuse(format!("{key}={n}: too large"));
            default
        })
    }

    /// A positive count of milliseconds.
    pub(crate) fn ms(&mut self, suffix: &str, default: Duration) -> Duration {
        let default = u64::try_from(default.as_millis()).unwrap_or(u64::MAX);
        Duration::from_millis(self.positive(suffix, default))
    }

    /// A comma-separated list of statuses from 400 to 599.
    pub(crate) fn statuses(&mut self, suffix: &str, default: &StatusSet) -> StatusSet {
        let Some(v) = self.stated(suffix) else { return default.clone() };
        StatusSet::parse(&v).unwrap_or_else(|why| {
            let key = self.key(suffix);
            self.refuse(format!("{key}={v:?}: {why}"));
            default.clone()
        })
    }

    /// `value` when nothing was refused; else every refusal, in the order
    /// read.
    pub(crate) fn finish<T>(self, value: T) -> Result<T, String> {
        if self.refused.is_empty() {
            Ok(value)
        } else {
            Err(self.refused.join("; "))
        }
    }
}
