//! The brokers the RabbitMQ CDR sink reaches its cluster through, and the
//! order a connect attempt tries them in. One attempt walks every broker from
//! a cursor, each given its share of the connect bound, so a dead or silent
//! node never keeps the sink down while another answers. The cursor starts
//! where the writer's seed points, so workers spread over the nodes; it stays
//! on a node whose session delivered, moves past one whose session failed,
//! and after a failed attempt starts past the last node it tried, so a node
//! that held an attempt to its deadline is not tried first again.

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::time::Duration;

use amqp_connect::broker_name;
use lapin::uri::AMQPUri;
use tokio::sync::watch;
use tokio::time::Instant;

use super::resolver::BrokerResolver;
use super::session::Session;
use super::settings::RabbitMqCdrSettings;
use super::tally::Tally;

/// One broker of the list and the resolution of its host, single-flight.
pub(super) struct Broker {
    pub(super) uri: AMQPUri,
    pub(super) resolver: BrokerResolver,
}

/// Why no broker of the list opened a session: each tried broker's failure,
/// in the order tried.
#[derive(Debug)]
pub(super) struct Unreached(Vec<String>);

impl std::fmt::Display for Unreached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.is_empty() {
            return f.write_str("no broker listed");
        }
        f.write_str(&self.0.join("; "))
    }
}

pub(super) struct Brokers {
    brokers: Vec<Broker>,
    /// The broker the next attempt starts at.
    next: usize,
    /// The broker the live session runs on, until [`Brokers::ended`].
    current: Option<usize>,
}

/// A start drawn per writer from the process's random hasher keys: writers of
/// different processes, and of one process, start on different brokers.
pub(super) fn drawn_start() -> usize {
    let drawn = RandomState::new().hash_one(0u8);
    usize::try_from(drawn % u64::from(u32::MAX)).unwrap_or(0)
}

impl Brokers {
    /// The brokers `uris` lists, the first attempt starting at `first`
    /// (modulo their count); a parked lookup answer older than `max_age` is
    /// not used.
    pub(super) fn new(uris: &[AMQPUri], first: usize, max_age: Duration) -> Self {
        let brokers = uris
            .iter()
            .map(|uri| Broker { uri: uri.clone(), resolver: BrokerResolver::new(max_age) })
            .collect();
        Self::of(brokers, first)
    }

    /// The list `brokers`, the first attempt starting at `first` (modulo
    /// their count).
    fn of(brokers: Vec<Broker>, first: usize) -> Self {
        let mut this = Self { brokers, next: 0, current: None };
        this.start_at(first);
        this
    }

    /// The next attempt starts at broker `first` (modulo the count).
    pub(super) fn start_at(&mut self, first: usize) {
        self.next = first.checked_rem(self.brokers.len()).unwrap_or(0);
    }

    /// One connect attempt by `deadline`: each broker from the cursor in
    /// turn, its host resolved, its addresses connected and its handshakes
    /// done within its one share of the time left, until one opens a session
    /// ([`Session::open`]). The cursor moves past each broker as it is tried,
    /// so a failed or dropped attempt leaves it past the last one tried; a
    /// session opened sets it to its broker.
    pub(super) async fn open(
        &mut self,
        settings: &RabbitMqCdrSettings,
        deadline: Instant,
        tally: &Tally,
        close_by: &watch::Sender<Option<Instant>>,
    ) -> Result<Session, Unreached> {
        let count = self.brokers.len();
        let start = self.next;
        self.current = None;
        let mut failures = Vec::new();
        for tried in 0..count {
            if tried > 0 && Instant::now() >= deadline {
                break;
            }
            let index = (start + tried) % count;
            self.start_at(index + 1);
            let Some(broker) = self.brokers.get_mut(index) else {
                break;
            };
            let later = count - tried - 1;
            let opened =
                Session::open(settings, broker, later, deadline, tally, close_by.subscribe()).await;
            match opened {
                Ok(session) => {
                    tracing::info!(broker = %broker_name(&broker.uri), "CDR broker connected");
                    self.next = index;
                    self.current = Some(index);
                    return Ok(session);
                }
                Err(e) => failures.push(format!("{}: {e}", broker_name(&broker.uri))),
            }
        }
        if let Some(untried) = count.checked_sub(failures.len()).filter(|n| *n > 0) {
            failures.push(format!("{untried} more not tried by the connect deadline"));
        }
        Err(Unreached(failures))
    }

    /// The live session ended: the next attempt starts at its broker when
    /// `stay` (it delivered, or its end tells nothing of the broker), else
    /// at the broker after it.
    pub(super) fn ended(&mut self, stay: bool) {
        if let Some(index) = self.current.take() {
            self.start_at(if stay { index } else { index + 1 });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{mpsc, Arc, Mutex};

    use b2bua::metrics::B2buaMetrics;

    use super::super::settings::{CdrDeliveryBounds, CdrQueueDeclare};
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    /// Two writers of one process drawing the same start is a coin toss: 64
    /// of them all on one of two brokers would be a fixed draw.
    #[test]
    fn writers_draw_starts_spread_over_the_brokers() {
        let starts: std::collections::HashSet<usize> = (0..64).map(|_| drawn_start() % 2).collect();
        assert_eq!(starts.len(), 2, "every writer drew the same start");
    }

    /// A broker named `host` whose lookup answers only once `gate` is
    /// released or dropped.
    fn gated(host: &str, gate: mpsc::Receiver<()>) -> Broker {
        let gate = Mutex::new(gate);
        let lookup = move |_: &str, _: u16| {
            let _ = gate.lock().unwrap().recv();
            Err(std::io::Error::other("released"))
        };
        Broker {
            uri: format!("amqp://{host}/%2f").parse().unwrap(),
            resolver: BrokerResolver::with_lookup(Arc::new(lookup), Duration::from_secs(1)),
        }
    }

    /// A loopback port bound and never listened on, and its address: every
    /// connect to it is refused while the socket lives, and no other socket
    /// of the box takes the port meanwhile.
    fn refusing() -> (socket2::Socket, std::net::SocketAddr) {
        use socket2::{Domain, Socket, Type};
        let socket = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
        socket.bind(&std::net::SocketAddr::from(([127, 0, 0, 1], 0)).into()).unwrap();
        let addr = socket.local_addr().unwrap().as_socket().unwrap();
        (socket, addr)
    }

    /// The broker at `addr`, its host a literal address.
    fn at(addr: std::net::SocketAddr) -> Broker {
        Broker {
            uri: format!("amqp://{addr}/%2f").parse().unwrap(),
            resolver: BrokerResolver::new(Duration::from_secs(1)),
        }
    }

    /// A connect bound whose tenth is a broker's share in a list of ten: the
    /// nine tenths left are far past any delay a loaded box adds.
    const TEN_SHARES: Duration = Duration::from_secs(3);

    /// `first`, then nine refusing brokers, the second of the list named:
    /// with a bound of [`TEN_SHARES`], `first`'s share is a tenth of it, and
    /// every broker after it is tried within the rest. The refusing ports are
    /// held while the sockets live.
    fn first_of_ten(first: Broker) -> (Vec<Broker>, std::net::SocketAddr, Vec<socket2::Socket>) {
        let (held, addrs): (Vec<_>, Vec<_>) = (0..9).map(|_| refusing()).unzip();
        let mut listed = vec![first];
        listed.extend(addrs.iter().copied().map(at));
        (listed, addrs[0], held)
    }

    /// The first broker's lookup never answers: it holds the attempt for its
    /// share of the connect bound, and the broker after it is tried within
    /// the rest.
    #[tokio::test]
    async fn a_lookup_that_never_answers_costs_its_share_and_the_next_broker_is_tried() {
        let (release, gate) = mpsc::channel();
        let (listed, next, _held) = first_of_ten(gated("silent.invalid", gate));
        let (failed, took) = attempt(listed, TEN_SHARES).await;
        drop(release);
        let said = failed.to_string();
        assert!(said.contains("silent.invalid:5672: broker host unresolved"), "{said}");
        assert!(said.contains(&format!("{next}: broker connect failed")), "{said}");
        assert!(!said.contains("not tried"), "{said}");
        let share = TEN_SHARES / 10;
        assert!(took >= share - 10 * MS, "the attempt took {took:?}: under the first's share");
    }

    /// The first broker's lookup answers after 4 s, then its AMQP handshake
    /// is never answered: its one share of the connect bound, 8 s, caps the
    /// lookup and the handshake together, where a share for each would cost
    /// 10 s; the broker after it is tried within the rest.
    #[tokio::test]
    #[ignore = "slow lane: real clock >= 1 s"]
    async fn a_broker_slow_in_two_phases_costs_one_share_of_the_bound() {
        let unanswering = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = unanswering.local_addr().unwrap();
        let lookup = move |_: &str, _: u16| {
            std::thread::sleep(Duration::from_secs(4));
            Ok(vec![addr])
        };
        let (_held, next) = refusing();
        let listed = vec![
            Broker {
                uri: "amqp://slow.invalid/%2f".parse().unwrap(),
                resolver: BrokerResolver::with_lookup(Arc::new(lookup), Duration::from_secs(1)),
            },
            at(next),
        ];
        let (failed, took) = attempt(listed, Duration::from_secs(16)).await;
        let said = failed.to_string();
        assert!(said.contains("slow.invalid:5672: AMQP handshake not done"), "{said}");
        assert!(said.contains(&format!("{next}: broker connect failed")), "{said}");
        let (share, two_shares) = (Duration::from_secs(8), Duration::from_secs(10));
        assert!(took >= share - 20 * MS, "the attempt took {took:?}: under one share");
        assert!(took < (share + two_shares) / 2, "the attempt took {took:?}: a share per phase");
    }

    /// The first broker's handshake is never answered and the bound is the
    /// least one try is given: that try takes it whole and no other broker is
    /// tried. The next attempt starts past the last broker tried, at the
    /// second; an attempt's first try is made whatever its deadline.
    #[tokio::test]
    async fn the_attempt_after_one_its_deadline_cut_starts_past_the_last_broker_tried() {
        let unanswering = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let stalled = unanswering.local_addr().unwrap();
        let ((_second_held, second), (_third_held, third)) = (refusing(), refusing());
        let mut brokers = Brokers::of(vec![at(stalled), at(second), at(third)], 0);
        let (cut, _) = attempt_of(&mut brokers, amqp_connect::MIN_TRY).await;
        let cut = cut.to_string();
        assert!(cut.starts_with(&format!("{stalled}: ")), "{cut}");
        assert!(cut.ends_with("2 more not tried by the connect deadline"), "{cut}");
        let (next, _) = attempt_of(&mut brokers, amqp_connect::MIN_TRY).await;
        let next = next.to_string();
        assert!(next.starts_with(&format!("{second}: broker connect failed")), "{next}");
    }

    /// The first broker refuses, the second, the last, stalls its handshake
    /// and takes the rest of a bound of seconds: the next attempt starts past
    /// the last broker tried, back at the first, not past the first tried.
    #[tokio::test]
    #[ignore = "slow lane: real clock >= 1 s"]
    async fn the_attempt_after_a_failed_one_starts_past_the_last_broker_tried_not_the_first() {
        let (_first_held, first) = refusing();
        let unanswering = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let stalled = unanswering.local_addr().unwrap();
        let mut brokers = Brokers::of(vec![at(first), at(stalled)], 0);
        let (failed, _) = attempt_of(&mut brokers, Duration::from_secs(3)).await;
        let failed = failed.to_string();
        assert!(failed.starts_with(&format!("{first}: broker connect failed")), "{failed}");
        assert!(failed.contains(&format!("{stalled}: ")), "both tried: {failed}");
        let (next, _) = attempt_of(&mut brokers, amqp_connect::MIN_TRY).await;
        let next = next.to_string();
        assert!(next.starts_with(&format!("{first}: broker connect failed")), "{next}");
    }

    /// One connect attempt over `listed` from the first, bounded by `bound`:
    /// why it failed, and how long it took.
    async fn attempt(listed: Vec<Broker>, bound: Duration) -> (Unreached, Duration) {
        attempt_of(&mut Brokers::of(listed, 0), bound).await
    }

    /// One connect attempt of `brokers` from their cursor, bounded by
    /// `bound`: why it failed, and how long it took.
    async fn attempt_of(brokers: &mut Brokers, bound: Duration) -> (Unreached, Duration) {
        let bounds = CdrDeliveryBounds { connect_timeout: bound, ..CdrDeliveryBounds::default() };
        let settings = RabbitMqCdrSettings {
            brokers: Vec::new(),
            queue: "cdr".into(),
            declare: CdrQueueDeclare::Own { max_len: 100 },
            bounds,
            tls: amqp_connect::BrokerTls::PLAIN,
        };
        let tally = Tally::new(B2buaMetrics::new());
        let close_by = watch::Sender::new(None);
        let t0 = Instant::now();
        let failed = brokers.open(&settings, t0 + bound, &tally, &close_by).await.err();
        (failed.expect("no broker serves"), t0.elapsed())
    }
}
