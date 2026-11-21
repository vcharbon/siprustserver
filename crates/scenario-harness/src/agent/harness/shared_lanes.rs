//! A session over a caller-built simulated fabric, one over a fabric that
//! holds chosen datagrams in flight, and a SUT endpoint on a lane of its own:
//! the seams several SUT instances serving one shared address, or a path
//! that reorders, need.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;

use layer_harness::{NetworkTag, TransportKind};
use sip_clock::Clock;
use sip_net::{BindUdpOpts, SignalingNetwork, UdpEndpoint};

use super::{Harness, RECV_TIMEOUT, SUT_QUEUE_MAX};

impl Harness {
    /// A session over a caller-built simulated fabric, which the caller may
    /// keep a clone of to steer it while the run goes on (a shared address,
    /// [`sip_net_testkit::SimulatedSignalingNetwork::share_address`]).
    pub fn with_simulated_network(
        scenario_name: impl Into<String>,
        net: sip_net_testkit::SimulatedSignalingNetwork,
    ) -> Self {
        let sim: Arc<dyn SignalingNetwork> = Arc::new(net);
        Self::build(scenario_name.into(), sim, Clock::test_at(0), TransportKind::Fake, RECV_TIMEOUT)
    }

    /// Like [`with_transit_delay`](Harness::with_transit_delay), plus a
    /// per-datagram hold on the simulated fabric: a datagram `delay` holds
    /// arrives that much later than the transit, so one its sender put on the
    /// wire after it may arrive first. The seam a scenario reorders a path's
    /// datagrams through while every party stays compliant at its send.
    pub fn with_transit_delay_and_send_delay(
        scenario_name: impl Into<String>,
        transit_delay_ms: u64,
        delay: sip_net_testkit::SendDelay,
    ) -> Self {
        let net = sip_net_testkit::SimulatedSignalingNetwork::new(transit_delay_ms.max(1));
        Self::with_simulated_network(scenario_name, net.with_send_delay(delay))
    }

    /// [`bind_sut_with_roles`](Self::bind_sut_with_roles) on the lane
    /// `addr#label`: one of several SUT endpoints serving one shared address
    /// keeps a ladder column and an audit stream of its own.
    pub async fn bind_sut_on_lane(
        &self,
        name: impl Into<String>,
        addr: &str,
        label: &str,
        roles: HashSet<sip_net::UaRole>,
    ) -> (Box<dyn UdpEndpoint>, SocketAddr) {
        let addr: SocketAddr = addr.parse().unwrap_or_else(|e| panic!("bad addr {addr:?}: {e}"));
        self.addrs.note_bound(addr);
        self.recorder.register_lane_keyed(format!("{addr}#{label}"), addr, name, NetworkTag::Core);
        let opts = BindUdpOpts::new(addr, SUT_QUEUE_MAX).with_roles(roles).with_lane_label(label);
        let ep =
            self.network.bind_udp(opts).await.unwrap_or_else(|e| panic!("bind {addr} failed: {e}"));
        (ep, addr)
    }
}
