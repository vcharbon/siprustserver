//! The broker name resolution of the RabbitMQ CDR sink: single-flight and
//! bounded by the connect deadline. A system lookup cannot be cancelled, so it
//! runs on a detached thread (never the runtime's blocking pool, whose drop
//! waits on it) and at most one runs at a time: an attempt whose deadline
//! passes leaves its lookup parked, and the next attempt awaits that lookup.
//! An answer that arrived while parked serves the next attempt only while it is
//! younger than the resolver's `max_age`; an older one is replaced by a fresh
//! lookup.

use std::io;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::oneshot::{self, error::TryRecvError};
use tokio::time::{timeout_at, Instant};

type Lookup = Arc<dyn Fn(&str, u16) -> io::Result<Vec<SocketAddr>> + Send + Sync>;
/// A lookup's result and when it arrived.
type Answer = oneshot::Receiver<(std::time::Instant, io::Result<Vec<SocketAddr>>)>;

pub(super) struct BrokerResolver {
    lookup: Lookup,
    max_age: Duration,
    /// The lookup parked by an attempt it outlived, until an attempt takes its
    /// answer.
    in_flight: Option<Answer>,
}

impl BrokerResolver {
    /// Resolves through the system resolver; a parked answer older than
    /// `max_age` is not used.
    pub(super) fn new(max_age: Duration) -> Self {
        Self::with_lookup(
            Arc::new(|host, port| Ok((host, port).to_socket_addrs()?.collect())),
            max_age,
        )
    }

    fn with_lookup(lookup: Lookup, max_age: Duration) -> Self {
        Self { lookup, max_age, in_flight: None }
    }

    /// The addresses of `host:port`: an IP literal (bracketed or not) at once,
    /// a name by the one lookup in flight, started when none is or when the
    /// parked one answered more than `max_age` ago. `TimedOut` when `deadline`
    /// passes first; the lookup stays parked for the next call, and dropping
    /// this future keeps it parked too.
    pub(super) async fn resolve(
        &mut self,
        host: &str,
        port: u16,
        deadline: Instant,
    ) -> io::Result<Vec<SocketAddr>> {
        if let Ok(ip) = host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>() {
            return Ok(vec![SocketAddr::new(ip, port)]);
        }
        let parked = match self.in_flight.take().map(|mut answer| (answer.try_recv(), answer)) {
            Some((Ok((at, answered)), _)) if at.elapsed() <= self.max_age => return answered,
            Some((Err(TryRecvError::Empty), answer)) => Some(answer),
            _ => None,
        };
        let answer = match parked {
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
        match answered {
            Ok((_, answered)) => answered,
            Err(_) => Err(io::Error::other(format!("{host} lookup thread ended unanswered"))),
        }
    }

    fn start(&self, host: &str, port: u16) -> io::Result<Answer> {
        let (tx, rx) = oneshot::channel();
        let lookup = self.lookup.clone();
        let host = host.to_owned();
        std::thread::Builder::new().name("cdr-broker-dns".into()).spawn(move || {
            let answered = lookup(&host, port);
            let _ = tx.send((std::time::Instant::now(), answered));
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
    fn gated(max_age: Duration) -> (BrokerResolver, Arc<AtomicUsize>, mpsc::Sender<()>) {
        let started = Arc::new(AtomicUsize::new(0));
        let (release, gate) = mpsc::channel::<()>();
        let gate = Mutex::new(gate);
        let count = started.clone();
        let resolver = BrokerResolver::with_lookup(
            Arc::new(move |_, _| {
                count.fetch_add(1, Ordering::SeqCst);
                gate.lock().unwrap().recv().map_err(io::Error::other)?;
                Ok(vec![ADDR])
            }),
            max_age,
        );
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
        let (mut resolver, started, release) = gated(Duration::from_secs(10));
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
        let (mut resolver, started, release) = gated(Duration::from_secs(10));
        let attempt = resolver.resolve("broker", 5672, later());
        assert!(tokio::time::timeout(Duration::from_millis(30), attempt).await.is_err());
        release.send(()).unwrap();
        assert_eq!(resolver.resolve("broker", 5672, later()).await.unwrap(), [ADDR]);
        assert_eq!(started.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_stale_parked_answer_is_replaced_by_a_fresh_lookup() {
        let (mut resolver, started, release) = gated(Duration::from_millis(10));
        assert!(resolver.resolve("broker", 5672, soon()).await.is_err());
        release.send(()).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        release.send(()).unwrap();
        assert_eq!(resolver.resolve("broker", 5672, later()).await.unwrap(), [ADDR]);
        assert_eq!(started.load(Ordering::SeqCst), 2, "the stale answer started a new lookup");
    }

    #[tokio::test]
    async fn an_ip_literal_starts_no_lookup() {
        let (mut resolver, started, _release) = gated(Duration::from_secs(10));
        let past = Instant::now();
        assert_eq!(resolver.resolve("192.0.2.7", 5672, past).await.unwrap(), [ADDR]);
        let v6 = resolver.resolve("[2001:db8::1]", 5672, past).await.unwrap();
        assert_eq!(v6, [SocketAddr::new("2001:db8::1".parse().unwrap(), 5672)]);
        assert_eq!(started.load(Ordering::SeqCst), 0);
    }
}
