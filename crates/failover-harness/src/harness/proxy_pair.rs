//! Two front proxy instances serving one address, as an ECMP next hop
//! spreads flows: both bind the shared address on the SIP fabric, so every
//! Via and Record-Route either writes names it, and each sender's flow to it
//! is delivered to the instance the test assigns. The instances share the
//! cookie key; each keeps its own worker registry, health view, Via-branch
//! sequence, dead notifier and node.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use observe::NodeScope;

use super::{FailoverHarness, ProxySut};

/// One instance of a [`ProxyPair`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Instance {
    A,
    B,
}

impl Instance {
    /// The instance's slot on the shared address: A binds first.
    fn member(self) -> usize {
        match self {
            Instance::A => 0,
            Instance::B => 1,
        }
    }

    /// The instance's lane and node name.
    fn label(self) -> &'static str {
        match self {
            Instance::A => "proxy-a",
            Instance::B => "proxy-b",
        }
    }

    /// The instance's Via-branch seed: two instances never mint one branch.
    fn id_seed(self) -> u64 {
        match self {
            Instance::A => 0xA0_C0FFEE,
            Instance::B => 0xB0_C0FFEE,
        }
    }
}

/// The two instances and the flow table the fabric delivers by. A sender
/// with no assignment reaches A.
pub struct ProxyPair {
    pub a: ProxySut,
    pub b: ProxySut,
    flows: Arc<Mutex<HashMap<SocketAddr, Instance>>>,
}

impl ProxyPair {
    /// The address both instances serve.
    pub fn addr(&self) -> SocketAddr {
        self.a.addr()
    }

    /// The instance `which`.
    pub fn instance(&self, which: Instance) -> &ProxySut {
        match which {
            Instance::A => &self.a,
            Instance::B => &self.b,
        }
    }

    /// From now on every datagram `sender` sends to the shared address
    /// reaches instance `to`.
    pub fn assign(&self, sender: SocketAddr, to: Instance) {
        self.flows.lock().unwrap().insert(sender, to);
    }
}

impl FailoverHarness {
    /// Stand up two load-balancing proxy instances serving `addr`, each over
    /// its own registry holding `workers` alive, with its own dead notifier
    /// and its own lane (`proxy-a`, `proxy-b`). Both lanes are relay lanes,
    /// role-tagged `{Proxy}`: each carries only the flows assigned to it, so
    /// conformance is judged at the parties.
    pub async fn spawn_proxy_pair(
        &mut self,
        addr: &str,
        workers: &[(&str, SocketAddr)],
    ) -> ProxyPair {
        let shared: SocketAddr = addr.parse().unwrap_or_else(|e| panic!("bad addr {addr:?}: {e}"));
        let flows: Arc<Mutex<HashMap<SocketAddr, Instance>>> = Arc::default();
        let table = flows.clone();
        self.sip_net.share_address(
            shared,
            Arc::new(move |src| {
                table.lock().unwrap().get(&src).copied().unwrap_or(Instance::A).member()
            }),
        );
        let a = self.spawn_pair_instance(addr, workers, Instance::A).await;
        let b = self.spawn_pair_instance(addr, workers, Instance::B).await;
        ProxyPair { a, b, flows }
    }

    /// Instance `which` of a pair at `addr`; A binds first, so it takes the
    /// shared address's first slot.
    async fn spawn_pair_instance(
        &mut self,
        addr: &str,
        workers: &[(&str, SocketAddr)],
        which: Instance,
    ) -> ProxySut {
        let label = which.label();
        let roles = HashSet::from([sip_net::UaRole::Proxy]);
        let (ep, sock) = self.harness.bind_sut_on_lane(label, addr, label, roles).await;
        self.relay_lanes.push(format!("{sock}#{label}"));
        let node = NodeScope::named(label);
        let parts = node.enter(|| {
            b2bua_harness::spawn_proxy_core_with(
                ep,
                sock,
                None,
                workers,
                self.clock.clone(),
                which.id_seed(),
            )
        });
        let b2bua_harness::ProxyCoreParts { addr: sock, registry, metrics, observer: _, task } =
            parts;
        let notices = self.spawn_dead_notifier(&node, sock, &registry).await;
        ProxySut {
            addr: sock,
            ext_addr: None,
            registry,
            metrics,
            task,
            probe_task: None,
            notices,
            node,
        }
    }
}

impl super::HarnessHandle {
    /// [`bind_sut_with_roles`](Self::bind_sut_with_roles) on the lane
    /// `addr#label`, for one of several SUT endpoints serving one address.
    async fn bind_sut_on_lane(
        &self,
        name: &str,
        addr: &str,
        label: &str,
        roles: HashSet<sip_net::UaRole>,
    ) -> (Box<dyn sip_net::UdpEndpoint>, SocketAddr) {
        let h = self.inner.lock().unwrap().take().expect("harness taken (already finished?)");
        let res = h.bind_sut_on_lane(name, addr, label, roles).await;
        *self.inner.lock().unwrap() = Some(h);
        res
    }
}
