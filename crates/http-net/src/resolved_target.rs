//! [`ResolvedTarget`] — where a client sends its requests.
//!
//! A target is a socket address, or a `host:port` name resolved on the
//! request path by a [`NameResolver`]: a name that does not resolve fails the
//! request like a transport error, inside the request's budget, so a peer
//! whose name resolves late is reached once it does. A name that parses as a
//! socket address (`10.0.0.1:8080`, `[::1]:8080`) is one, never looked up.
//!
//! One lookup is in flight at a time, and every request arriving meanwhile
//! waits for it within its own budget; a lookup outlives the requests that
//! gave up on it and lands for the next. A lookup is bounded
//! ([`LookupSettings::bound`], [`LOOKUP_BOUND`] by default): past it, its
//! waiters get no address and the slot is freed, while the stuck lookup runs
//! on until it ends by itself (a blocking resolver thread cannot be
//! cancelled) and its late answer is dropped. While
//! [`LookupSettings::max_stuck`] lookups of one target are stuck so
//! ([`MAX_STUCK`] by default), no new one starts and a request gets no
//! address at once.
//!
//! The addresses a name resolves to are kept until
//! [`ResolvedTarget::forget`]; the next request then looks the name up
//! again. [`ResolvedTarget::refresh`] asks for a fresh lookup but serves
//! stale: the next request starts the lookup and is answered the addresses
//! kept at once, never waiting for it; a lookup that finds something
//! replaces them, one that finds nothing (no answer, past its bound, too many
//! stuck) leaves them in use until the next refresh. A lookup only starts
//! while no fresh address is kept, so the one in flight at a forget is
//! already a fresh one: it lands and is kept.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::watch;

use crate::poison::{locked, RESOLVED_NAME};

use crate::resolver::NameResolver;

/// The longest one lookup is waited for, unless told otherwise.
pub const LOOKUP_BOUND: Duration = Duration::from_secs(2);

/// The most lookups of one target left running past their bound before no
/// new lookup starts, unless told otherwise.
pub const MAX_STUCK: usize = 4;

/// How a target's name lookups are bounded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LookupSettings {
    /// The longest one lookup is waited for.
    pub bound: Duration,
    /// The most lookups left running past their bound before no new lookup
    /// starts.
    pub max_stuck: usize,
}

impl Default for LookupSettings {
    fn default() -> Self {
        Self { bound: LOOKUP_BOUND, max_stuck: MAX_STUCK }
    }
}

/// The addresses a name resolved to, in the resolver's order; never empty.
pub type Addresses = Arc<[SocketAddr]>;

/// A peer's address, or the name that resolves to it.
pub struct ResolvedTarget {
    kind: Kind,
}

enum Kind {
    Addr(SocketAddr),
    Name(Arc<Named>),
}

/// A lookup's answer as its waiters see it: `None` while it runs.
type Answer = Option<Option<Addresses>>;

struct Named {
    name: String,
    resolver: Arc<dyn NameResolver>,
    lookup: LookupSettings,
    state: Mutex<NameState>,
}

#[derive(Default)]
struct NameState {
    /// The addresses kept.
    addrs: Option<Addresses>,
    /// The addresses kept are due a fresh lookup; they serve until one
    /// lands.
    stale: bool,
    /// The lookup in flight, if any.
    lookup: Option<watch::Receiver<Answer>>,
    /// Lookups still running past their bound.
    stuck: usize,
}

/// What a request finds: the addresses kept, the lookup to wait for, or
/// nothing to wait for.
enum Found {
    Kept(Addresses),
    Wait(watch::Receiver<Answer>),
    Refused,
}

impl Named {
    /// The state. Every step leaves it whole, so a poisoned lock is taken
    /// as it is.
    fn lock(&self) -> MutexGuard<'_, NameState> {
        locked(&self.state, RESOLVED_NAME)
    }

    /// The addresses kept (a stale set starting its fresh lookup behind
    /// the caller), or the lookup in flight, started when none is and fewer
    /// than [`MAX_STUCK`] are stuck.
    fn address_or_lookup(self: &Arc<Self>) -> Found {
        let mut state = self.lock();
        if let Some(addrs) = state.addrs.clone() {
            if state.stale && state.lookup.is_none() {
                if state.stuck >= self.lookup.max_stuck {
                    state.stale = false;
                } else {
                    let _ = self.start(&mut state);
                }
            }
            return Found::Kept(addrs);
        }
        if let Some(waiting) = state.lookup.as_ref() {
            return Found::Wait(waiting.clone());
        }
        if state.stuck >= self.lookup.max_stuck {
            return Found::Refused;
        }
        Found::Wait(self.start(&mut state))
    }

    /// Start a lookup, its answer landing for every request after it.
    fn start(self: &Arc<Self>, state: &mut NameState) -> watch::Receiver<Answer> {
        let (tx, rx) = watch::channel(None);
        state.lookup = Some(rx.clone());
        let done = LookupDone(self.clone());
        tokio::spawn(async move {
            let found = done.run().await;
            done.land(found.clone());
            drop(done);
            let _ = tx.send(Some(found));
        });
        rx
    }
}

/// The lookup in flight. Landing keeps the addresses it found; ending, a
/// panicking one included, frees the slot so the next request starts another.
struct LookupDone(Arc<Named>);

impl LookupDone {
    /// Run the lookup within its bound; one that overruns is left running,
    /// counted stuck until it ends.
    async fn run(&self) -> Option<Addresses> {
        let named = self.0.clone();
        let mut lookup = tokio::spawn(async move { named.resolver.resolve(&named.name).await });
        match tokio::time::timeout(self.0.lookup.bound, &mut lookup).await {
            Ok(Ok(found)) => (!found.is_empty()).then(|| found.into()),
            Ok(Err(_)) => None,
            Err(_) => {
                self.0.lock().stuck += 1;
                let stuck = Stuck(self.0.clone());
                tokio::spawn(async move {
                    let _ = lookup.await;
                    drop(stuck);
                });
                None
            }
        }
    }

    /// Keep what the lookup found; a lookup that found nothing leaves the
    /// stale addresses in use until the next refresh.
    fn land(&self, found: Option<Addresses>) {
        let mut state = self.0.lock();
        state.stale = false;
        if let Some(addrs) = found {
            state.addrs = Some(addrs);
        }
    }
}

impl Drop for LookupDone {
    fn drop(&mut self) {
        self.0.lock().lookup = None;
    }
}

/// A lookup running past its bound; it stops counting when it ends.
struct Stuck(Arc<Named>);

impl Drop for Stuck {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        state.stuck = state.stuck.saturating_sub(1);
    }
}

impl ResolvedTarget {
    /// A target at `addr`.
    pub fn addr(addr: SocketAddr) -> Self {
        Self { kind: Kind::Addr(addr) }
    }

    /// A target named `host:port`, resolved by `resolver` on the first
    /// request that needs it and on every request until it resolves; a
    /// socket address is taken as it is.
    pub fn name_with(name: impl Into<String>, resolver: Arc<dyn NameResolver>) -> Self {
        Self::name_with_lookup(name, resolver, LookupSettings::default())
    }

    /// [`name_with`](Self::name_with), its lookups bounded by `lookup`.
    pub fn name_with_lookup(
        name: impl Into<String>,
        resolver: Arc<dyn NameResolver>,
        lookup: LookupSettings,
    ) -> Self {
        let name = name.into();
        if let Ok(addr) = name.parse::<SocketAddr>() {
            return Self::addr(addr);
        }
        Self {
            kind: Kind::Name(Arc::new(Named {
                name,
                resolver,
                lookup,
                state: Mutex::new(NameState::default()),
            })),
        }
    }

    /// Every address to send to; `None` when the name does not resolve.
    /// Waits for the lookup in flight, or starts one; a lookup is bounded,
    /// and a caller that gives up sooner leaves it to finish for the next. A
    /// name with addresses kept answers them at once, stale ones included.
    pub async fn resolve_all(&self) -> Option<Addresses> {
        let named = match &self.kind {
            Kind::Addr(addr) => return Some(Arc::from([*addr])),
            Kind::Name(named) => named,
        };
        let mut lookup = match named.address_or_lookup() {
            Found::Kept(addrs) => return Some(addrs),
            Found::Wait(lookup) => lookup,
            Found::Refused => return None,
        };
        let answer = lookup.wait_for(Option::is_some).await.ok()?.clone().flatten();
        answer.or_else(|| named.lock().addrs.clone())
    }

    /// The first address to send to; `None` when the name does not resolve
    /// ([`resolve_all`](Self::resolve_all)).
    pub async fn resolve(&self) -> Option<SocketAddr> {
        self.resolve_all().await.and_then(|addrs| addrs.first().copied())
    }

    /// Every address kept now, without a lookup.
    pub fn addresses(&self) -> Option<Addresses> {
        match &self.kind {
            Kind::Addr(addr) => Some(Arc::from([*addr])),
            Kind::Name(named) => named.lock().addrs.clone(),
        }
    }

    /// Start a lookup when the name has no fresh address and none is in
    /// flight, without waiting for it: it lands for the next request.
    pub fn look_up(&self) {
        if let Kind::Name(named) = &self.kind {
            let _ = named.address_or_lookup();
        }
    }

    /// The first address kept now, without a lookup.
    pub fn address(&self) -> Option<SocketAddr> {
        match &self.kind {
            Kind::Addr(addr) => Some(*addr),
            Kind::Name(named) => {
                named.lock().addrs.as_ref().and_then(|addrs| addrs.first().copied())
            }
        }
    }

    /// Forget the addresses a name resolved to: the next request looks the
    /// name up again. A lookup in flight lands and is kept. A socket address
    /// is kept.
    pub fn forget(&self) {
        if let Kind::Name(named) = &self.kind {
            let mut state = named.lock();
            state.addrs = None;
            state.stale = false;
        }
    }

    /// Look the name up again from the next request on, which is answered
    /// the addresses kept at once; they serve until a lookup finds others. A
    /// socket address is kept.
    pub fn refresh(&self) {
        if let Kind::Name(named) = &self.kind {
            let mut state = named.lock();
            state.stale = state.addrs.is_some();
        }
    }

    /// Whether a [`refresh`](Self::refresh) is still waiting for its lookup
    /// to be over (landed, past its bound, or refused by the stuck cap).
    pub fn is_refreshing(&self) -> bool {
        match &self.kind {
            Kind::Addr(_) => false,
            Kind::Name(named) => named.lock().stale,
        }
    }

    /// The host part of the target as a URL authority writes it, and its
    /// port.
    pub fn host_port(&self) -> (String, u16) {
        match &self.kind {
            Kind::Addr(addr) => match addr {
                SocketAddr::V4(v4) => (v4.ip().to_string(), v4.port()),
                SocketAddr::V6(v6) => (format!("[{}]", v6.ip()), v6.port()),
            },
            Kind::Name(named) => match named.name.rsplit_once(':') {
                Some((host, port)) => (host.to_string(), port.parse().unwrap_or(0)),
                None => (named.name.clone(), 0),
            },
        }
    }
}

impl std::fmt::Display for ResolvedTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            Kind::Addr(addr) => addr.fmt(f),
            Kind::Name(named) => f.write_str(&named.name),
        }
    }
}

#[cfg(test)]
mod tests;
