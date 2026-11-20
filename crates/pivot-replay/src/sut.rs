//! The settle oracles: how the interpreter's `Sut` contract reads each
//! deployment. Two, because there are two deployment shapes: a B2BUA worker
//! this process owns on the simulated lane, and a system shared by every cell
//! of a batch. An oracle answers honestly where it cannot read something (an
//! `Err` for every observable) so a document that asserts one fails loudly
//! instead of passing blind. Neither reads a call record: a cell's
//! call-records claim is answered after the call, by its judge.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use b2bua_harness::B2buaSut;
use pivot_interpreter::Sut;

/// The simulated lane's oracle: the in-process B2BUA worker.
pub struct WorkerSut<'a>(pub &'a B2buaSut);

impl Sut for WorkerSut<'_> {
    fn active_calls(&self) -> usize {
        self.0.active_calls()
    }

    /// Reads the worker's in-process metrics registry by Prometheus sample
    /// name, the same names a hosted lane scrapes over `/metrics`. A name the
    /// registry does not render is `Err`: "I could not read it", so an
    /// `absent` check can never pass on a typo.
    fn observe(&self, name: &str) -> Result<Option<String>, String> {
        let body = self.0.metrics().prometheus_text();
        sample(&body, name)
            .map(Some)
            .ok_or_else(|| format!("{name:?} is not an observable the simulated lane publishes"))
    }
}

/// A SHARED SUT's oracle: one process serving a whole batch of cells.
///
/// `active_calls` answers ZERO, always, and that is not a claim about this call:
/// the gauge counts every cell in flight, so a shared SUT can neither settle one
/// case on it nor say whether one case's call was reaped. Settling therefore
/// waits on the flow and the wire alone, and the post-run cleanup check is the
/// BATCH's, the scope that can read the gauge as a fact. It is relocated, never
/// disabled.
///
/// Observables read the SUT's own `/metrics` where the lane publishes any; a
/// lane that publishes none answers `Err` for every name.
pub struct HostedSut {
    pub metrics: Option<SocketAddr>,
}

impl Sut for HostedSut {
    fn active_calls(&self) -> usize {
        0
    }

    /// Reads the SUT's `/metrics` body by Prometheus sample name. An
    /// unreachable endpoint, an unrendered name, or a lane that publishes
    /// nothing is `Err` — an observable that cannot be read is not a passing one.
    fn observe(&self, name: &str) -> Result<Option<String>, String> {
        let metrics = self
            .metrics
            .ok_or_else(|| format!("{name:?} is not an observable this lane publishes"))?;
        let body = scrape(metrics)
            .ok_or_else(|| format!("the SUT's /metrics endpoint at {metrics} did not answer"))?;
        sample(&body, name)
            .map(Some)
            .ok_or_else(|| format!("{name:?} is not a sample the SUT renders"))
    }
}

/// The SUT's `b2bua_active_calls`, where its `/metrics` answers one.
pub fn active_calls(metrics: SocketAddr) -> Option<u64> {
    sample(&scrape(metrics)?, "b2bua_active_calls")?.parse().ok()
}

/// Each of `names` read off one scrape of the SUT's `/metrics`, in order;
/// `None` where the endpoint does not answer or renders one of them as no
/// whole number.
pub fn gauges(metrics: SocketAddr, names: &[&str]) -> Option<Vec<u64>> {
    let body = scrape(metrics)?;
    names.iter().map(|name| sample(&body, name)?.parse().ok()).collect()
}

/// A SUT's `/metrics` body, over a blocking HTTP/1.0 GET (a localhost round
/// trip, bounded by the timeouts here).
fn scrape(metrics: SocketAddr) -> Option<String> {
    get(metrics, "/metrics")
}

/// The whole reply to a blocking HTTP/1.0 `GET path` at one of the SUT's
/// HTTP listeners, head included; `None` where it could not be read.
pub fn get(at: SocketAddr, path: &str) -> Option<String> {
    let mut stream = TcpStream::connect_timeout(&at, Duration::from_millis(500)).ok()?;
    stream.set_read_timeout(Some(Duration::from_millis(500))).ok()?;
    stream.write_all(format!("GET {path} HTTP/1.0\r\nHost: localhost\r\n\r\n").as_bytes()).ok()?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply).ok()?;
    Some(reply)
}

/// One sample by name off a Prometheus text body. An exact name (labels
/// included, if the name carries any) answers its own sample; a bare name
/// whose family renders only labelled series answers the family sum. `None`
/// when the body carries no such name.
pub fn sample(body: &str, name: &str) -> Option<String> {
    if let Some(value) = body
        .lines()
        .find_map(|line| line.strip_prefix(name).and_then(|rest| rest.strip_prefix(' ')))
    {
        return Some(value.trim().to_string());
    }
    let family = format!("{name}{{");
    let mut sum = 0f64;
    let mut found = false;
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix(&family) {
            if let Some((_, value)) = rest.rsplit_once(' ') {
                if let Ok(v) = value.trim().parse::<f64>() {
                    sum += v;
                    found = true;
                }
            }
        }
    }
    found.then(|| if sum.fract() == 0.0 { format!("{}", sum as i64) } else { sum.to_string() })
}

#[cfg(test)]
mod tests {
    use super::sample;

    #[test]
    fn sample_reads_bare_names_labelled_families_and_refuses_unknown() {
        let body = "# HELP b2bua_active_calls live calls\n# TYPE b2bua_active_calls gauge\n\
                    b2bua_active_calls 3\n\
                    b2bua_requests_total{method=\"INVITE\"} 2\n\
                    b2bua_requests_total{method=\"BYE\"} 1\n";
        assert_eq!(sample(body, "b2bua_active_calls").as_deref(), Some("3"));
        assert_eq!(sample(body, "b2bua_requests_total").as_deref(), Some("3"));
        assert_eq!(sample(body, "b2bua_requests_total{method=\"BYE\"}").as_deref(), Some("1"));
        assert_eq!(sample(body, "b2bua_active"), None, "a name prefix is not a name");
        assert_eq!(sample(body, "sip_transactions_orphaned_total"), None);
    }
}
