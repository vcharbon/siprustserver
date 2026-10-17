//! The broker name resolution of the RabbitMQ CDR sink: single-flight and
//! bounded by the connect deadline. A system lookup cannot be cancelled, so it
//! runs on a detached thread (never the runtime's blocking pool, whose drop
//! waits on it) and at most one runs at a time: an attempt whose deadline
//! passes leaves its lookup parked, and the next attempt awaits that lookup.

use std::io;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;

use tokio::sync::oneshot;
use tokio::time::{timeout_at, Instant};

type Lookup = Arc<dyn Fn(&str, u16) -> io::Result<Vec<SocketAddr>> + Send + Sync>;
type Answer = oneshot::Receiver<io::Result<Vec<SocketAddr>>>;

pub(super) struct BrokerResolver {
    lookup: Lookup,
    /// The lookup still running, kept across attempts until it answers.
    in_flight: Option<Answer>,
}

impl Default for BrokerResolver {
    fn default() -> Self {
        Self::with_lookup(Arc::new(|host, port| Ok((host, port).to_socket_addrs()?.collect())))
    }
}

impl BrokerResolver {
    fn with_lookup(lookup: Lookup) -> Self {
        Self { lookup, in_flight: None }
    }

    /// The addresses of `host:port`: an IP literal (bracketed or not) at once,
    /// a name by the one lookup in flight, started when none is. `TimedOut`
    /// when `deadline` passes first; the lookup stays parked for the next call,
    /// and dropping this future keeps it parked too.
    pub(super) async fn resolve(
        &mut self,
        host: &str,
        port: u16,
        deadline: Instant,
    ) -> io::Result<Vec<SocketAddr>> {
        if let Ok(ip) = host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>() {
            return Ok(vec![SocketAddr::new(ip, port)]);
        }
        let answer = match self.in_flight.take() {
            Some(answer) => answer,
            None => self.start(host, port)?,
        };
        let answer = self.in_flight.insert(answer);
        let answered = timeout_at(deadline, answer).await.map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{host} still resolving at the connect deadline"),
            )
        })?;
        self.in_flight = None;
        answered.unwrap_or_else(|_| {
            Err(io::Error::other(format!("{host} lookup thread ended unanswered")))
        })
    }

    fn start(&self, host: &str, port: u16) -> io::Result<Answer> {
        let (tx, rx) = oneshot::channel();
        let lookup = self.lookup.clone();
        let host = host.to_owned();
        std::thread::Builder::new().name("cdr-broker-dns".into()).spawn(move || {
            let _ = tx.send(lookup(&host, port));
        })?;
        Ok(rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Mutex};
    use std::time::Duration;

    const ADDR: SocketAddr =
        SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::new(192, 0, 2, 7)), 5672);

    /// A lookup that answers `ADDR` once released, one release per lookup.
    fn gated() -> (BrokerResolver, Arc<AtomicUsize>, mpsc::Sender<()>) {
        let started = Arc::new(AtomicUsize::new(0));
        let (release, gate) = mpsc::channel::<()>();
        let gate = Mutex::new(gate);
        let count = started.clone();
        let resolver = BrokerResolver::with_lookup(Arc::new(move |_, _| {
            count.fetch_add(1, Ordering::SeqCst);
            gate.lock().unwrap().recv().map_err(io::Error::other)?;
            Ok(vec![ADDR])
        }));
        (resolver, started, release)
    }

    fn soon() -> Instant {
        Instant::now() + Duration::from_millis(30)
    }

    fn later() -> Instant {
        Instant::now() + Duration::from_secs(10)
    }

    #[tokio::test]
    async fn a_stalled_lookup_is_awaited_again_never_started_twice() {
        let (mut resolver, started, release) = gated();
        for _ in 0..3 {
            let err = resolver.resolve("broker", 5672, soon()).await.expect_err("stalled");
            assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        }
        assert_eq!(started.load(Ordering::SeqCst), 1, "one lookup in flight across attempts");
        release.send(()).unwrap();
        assert_eq!(resolver.resolve("broker", 5672, later()).await.unwrap(), [ADDR]);
        assert_eq!(started.load(Ordering::SeqCst), 1, "the parked lookup answered");
        release.send(()).unwrap();
        assert_eq!(resolver.resolve("broker", 5672, later()).await.unwrap(), [ADDR]);
        assert_eq!(started.load(Ordering::SeqCst), 2, "an answered lookup is not reused");
    }

    #[tokio::test]
    async fn a_dropped_attempt_keeps_its_lookup_parked() {
        let (mut resolver, started, release) = gated();
        let attempt = resolver.resolve("broker", 5672, later());
        assert!(tokio::time::timeout(Duration::from_millis(30), attempt).await.is_err());
        release.send(()).unwrap();
        assert_eq!(resolver.resolve("broker", 5672, later()).await.unwrap(), [ADDR]);
        assert_eq!(started.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_ip_literal_starts_no_lookup() {
        let (mut resolver, started, _release) = gated();
        let past = Instant::now();
        assert_eq!(resolver.resolve("192.0.2.7", 5672, past).await.unwrap(), [ADDR]);
        let v6 = resolver.resolve("[2001:db8::1]", 5672, past).await.unwrap();
        assert_eq!(v6, [SocketAddr::new("2001:db8::1".parse().unwrap(), 5672)]);
        assert_eq!(started.load(Ordering::SeqCst), 0);
    }
}
