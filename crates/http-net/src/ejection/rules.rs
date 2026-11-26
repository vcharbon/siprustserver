//! [`EjectionRules`] — which answers close the connection they came on, and
//! how many such closes a client makes a second; [`StatusSet`], the HTTP
//! statuses a rule names.

use std::fmt;

/// HTTP statuses from 400 to 599, each once, in ascending order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusSet(Vec<u16>);

/// The lowest status a [`StatusSet`] holds.
const LOWEST: u16 = 400;
/// The highest status a [`StatusSet`] holds.
const HIGHEST: u16 = 599;

impl StatusSet {
    /// The set of `codes`; `None` when one is outside 400–599.
    pub fn of(codes: &[u16]) -> Option<Self> {
        if codes.iter().any(|c| !(LOWEST..=HIGHEST).contains(c)) {
            return None;
        }
        Some(Self::sorted(codes.to_vec()))
    }

    fn sorted(mut codes: Vec<u16>) -> Self {
        codes.sort_unstable();
        codes.dedup();
        Self(codes)
    }

    /// The set a comma-separated list of codes states, blanks around each
    /// code ignored; `Err` naming the first entry that is not a code from 400
    /// to 599.
    pub fn parse(list: &str) -> Result<Self, String> {
        let mut codes = Vec::new();
        for entry in list.split(',').map(str::trim) {
            match entry.parse::<u16>() {
                Ok(code) if (LOWEST..=HIGHEST).contains(&code) => codes.push(code),
                _ => return Err(format!("{entry:?} is not a status from {LOWEST} to {HIGHEST}")),
            }
        }
        Ok(Self::sorted(codes))
    }

    /// Whether `status` is in the set.
    pub fn contains(&self, status: u16) -> bool {
        self.0.binary_search(&status).is_ok()
    }

    /// Whether no status is in both sets.
    pub fn is_disjoint(&self, other: &StatusSet) -> bool {
        !self.0.iter().any(|c| other.contains(*c))
    }
}

impl fmt::Display for StatusSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for code in &self.0 {
            if !first {
                f.write_str(",")?;
            }
            write!(f, "{code}")?;
            first = false;
        }
        Ok(())
    }
}

/// The statuses whose first answer closes its connection unless told
/// otherwise: a backend that is broken.
pub const DEFAULT_CLOSE_STATUSES: [u16; 3] = [500, 502, 504];

/// The statuses whose answers in a row close their connection unless told
/// otherwise: a backend shedding load.
pub const DEFAULT_SHED_STATUSES: [u16; 3] = [408, 429, 503];

/// How many shed answers in a row close their connection unless told
/// otherwise.
pub const DEFAULT_SHED_CLOSE_AFTER: u32 = 3;

/// Which answers close the connection they came on, within a budget of
/// closes a second per client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EjectionRules {
    /// An answer of one of these closes its connection.
    pub close_statuses: StatusSet,
    /// [`shed_close_after`](Self::shed_close_after) answers in a row of one
    /// of these close their connection; any other answer on the connection
    /// starts the run again. Disjoint from
    /// [`close_statuses`](Self::close_statuses).
    pub shed_statuses: StatusSet,
    /// The run of shed answers that closes a connection, at least 1.
    pub shed_close_after: u32,
    /// The closes these rules make, at most, per second and client
    /// ([`crate::ErrorCloses`]); past it the connection is pooled. `0` never
    /// closes one.
    pub closes_per_s: u32,
}

impl Default for EjectionRules {
    fn default() -> Self {
        Self {
            close_statuses: StatusSet(DEFAULT_CLOSE_STATUSES.to_vec()),
            shed_statuses: StatusSet(DEFAULT_SHED_STATUSES.to_vec()),
            shed_close_after: DEFAULT_SHED_CLOSE_AFTER,
            closes_per_s: crate::error_close::DEFAULT_ERROR_CLOSES_PER_S,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_list_reads_into_a_sorted_set_and_prints_back() {
        let set = StatusSet::parse(" 504, 500 ,502,500").unwrap();
        assert_eq!(set, StatusSet::of(&DEFAULT_CLOSE_STATUSES).unwrap());
        assert_eq!(set.to_string(), "500,502,504");
        assert!(set.contains(502) && !set.contains(503));
    }

    #[test]
    fn an_entry_that_is_not_a_status_from_400_to_599_is_named() {
        for (list, entry) in [("500,399", "\"399\""), ("600", "\"600\""), ("500,,502", "\"\"")] {
            let e = StatusSet::parse(list).unwrap_err();
            assert!(e.starts_with(entry), "{list}: {e}");
        }
        assert_eq!(StatusSet::of(&[200]), None);
    }

    #[test]
    fn the_default_sets_are_disjoint() {
        let rules = EjectionRules::default();
        assert!(rules.close_statuses.is_disjoint(&rules.shed_statuses));
        assert!(!rules.close_statuses.is_disjoint(&StatusSet::of(&[503, 504]).unwrap()));
        assert_eq!(rules.shed_statuses.to_string(), "408,429,503");
    }
}
