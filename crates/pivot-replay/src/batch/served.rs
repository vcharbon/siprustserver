//! What the batch's backend double served, for the end line: how many
//! exchanges, and how long it took from each request's receipt to its answer.

use serde::Serialize;

/// The backend double's record, summed up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Served {
    /// Every exchange it recorded, answered or not.
    pub served: usize,
    /// The process time of the exchanges that state one, in µs; absent where
    /// none does.
    pub process_us: Option<Quantiles>,
}

/// Nearest-rank quantiles of a set of µs durations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Quantiles {
    pub p50: u64,
    pub p99: u64,
    pub max: u64,
}

/// The record whose exchanges each state `process_us`, summed up.
pub fn of(process_us: &[Option<u64>]) -> Served {
    let mut times: Vec<u64> = process_us.iter().flatten().copied().collect();
    times.sort_unstable();
    Served { served: process_us.len(), process_us: quantiles(&times) }
}

/// The nearest-rank p50, p99 and max of `sorted`, ascending.
fn quantiles(sorted: &[u64]) -> Option<Quantiles> {
    let max = *sorted.last()?;
    let rank =
        |q: f64| sorted[((q * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len()) - 1];
    Some(Quantiles { p50: rank(0.50), p99: rank(0.99), max })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_every_exchange_and_ranks_the_timed_ones() {
        let mut times: Vec<Option<u64>> = (1..=200).rev().map(Some).collect();
        times.push(None);
        let served = of(&times);
        assert_eq!(served.served, 201);
        assert_eq!(served.process_us, Some(Quantiles { p50: 100, p99: 198, max: 200 }));
    }

    #[test]
    fn states_no_quantiles_where_no_exchange_is_timed() {
        assert_eq!(of(&[None]), Served { served: 1, process_us: None });
        assert_eq!(of(&[]), Served { served: 0, process_us: None });
        assert_eq!(of(&[Some(7)]).process_us, Some(Quantiles { p50: 7, p99: 7, max: 7 }));
    }
}
