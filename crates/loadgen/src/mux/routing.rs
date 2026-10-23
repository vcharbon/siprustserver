//! What a scenario declares about a call before its agents bind: the
//! correlation token, the binds per address in bind order, and how a shared
//! socket assigns each arriving leg.

use std::collections::HashMap;
use std::net::SocketAddr;

use scenario_harness::claim::ClaimRule;
use scenario_harness::legpick::LegPicker;

/// One declared bind on a mux endpoint, dispensed in declaration order as the
/// call's agents call `bind_udp(addr)`.
#[derive(Clone)]
pub(super) enum BindDecl {
    /// A caller (UAC) endpoint — registers its dialog from its first outbound
    /// request; holds no token-slot receiver. Declared explicitly only on a
    /// SHARED-vantage socket (caller + pending UAS receivers on one addr).
    Caller,
    /// A pending UAS receiver: `label` = the agent name; `claim` = the
    /// scenario-declared rule that assigns its inbound initial INVITE
    /// (`None` = legacy count/picker resolution).
    Uas { label: String, claim: Option<ClaimRule> },
}

/// The per-call routing a scenario declares before its agents bind: the single
/// correlation token every leg of the call carries, the binds in **bind
/// order** per addr (caller and/or UAS receivers — several on one `addr` form
/// a shared socket), and how a shared socket assigns each arriving leg: a
/// compiled [`LegPicker`], or per-leg data [`ClaimRule`]s (a scenario built
/// from capture data at case start) — never both on one socket. The mux never
/// reads `X-Api-Call` or any URI to route legs — that is the scenario's job,
/// expressed here and in how it dials the SUT.
#[derive(Clone, Default)]
pub struct CallRouting {
    pub(super) token: String,
    pub(super) binds: Vec<(SocketAddr, BindDecl)>,
    pub(super) pickers: HashMap<SocketAddr, LegPicker>,
}

impl CallRouting {
    /// Start a routing for a call with correlation token `token`.
    pub fn new(token: impl Into<String>) -> Self {
        Self { token: token.into(), binds: Vec::new(), pickers: HashMap::new() }
    }
    /// Declare a callee leg: an agent labelled `label` binds on `addr`. Declare
    /// legs in the order the driver binds the agents (several on one `addr` form
    /// a shared socket).
    pub fn leg(mut self, addr: SocketAddr, label: impl Into<String>) -> Self {
        self.binds.push((addr, BindDecl::Uas { label: label.into(), claim: None }));
        self
    }
    /// Declare a pending-UAS claim: the agent labelled `label` binds on `addr`
    /// and owns the inbound initial INVITE `rule` selects within this call's
    /// token scope. Consumed once; an INVITE matching no pending claim is
    /// counted `unclaimed` (never misdelivered). Same bind-order contract as
    /// [`leg`](Self::leg); claims and a [`picker`](Self::picker) are mutually
    /// exclusive per socket, as are claim and non-claim legs.
    pub fn claim(mut self, addr: SocketAddr, label: impl Into<String>, rule: ClaimRule) -> Self {
        self.binds.push((addr, BindDecl::Uas { label: label.into(), claim: Some(rule) }));
        self
    }
    /// Declare a caller (UAC) bind on `addr` — needed only when the caller
    /// SHARES a socket with pending UAS receivers (the pcap mux vantage, where
    /// the SUT dials new legs back to the ip:port that originated leg A). On
    /// an addr with any declared bind, EVERY bind must be declared, in bind
    /// order; an addr with none keeps its endpoint-role default.
    pub fn caller(mut self, addr: SocketAddr) -> Self {
        self.binds.push((addr, BindDecl::Caller));
        self
    }
    /// Attach the scenario-owned picker for a socket that carries >1 receiver.
    pub fn picker(mut self, addr: SocketAddr, picker: LegPicker) -> Self {
        self.pickers.insert(addr, picker);
        self
    }
}
