//! The scenario DSL — scenarios as **data** (port of the *useful* half of
//! `src/test-harness/framework/dsl.ts`, MIGRATION_PLAN_B2B §4(ii) decision B).
//!
//! The source DSL is a fluent builder (`alice.invite(...).expect(200).ack()`)
//! over a two-phase recorder/interpreter that maintains its own dialog state
//! (CSeq, route sets, tags, offer/answer) and its own `trace`. That machinery
//! *is* the transaction + call-context layers, which are not ported yet, and
//! there is no SUT to drive against. So we keep only what is load-bearing now:
//!
//!   - **named agents** (`alice`, `bob`) bound to wire addresses,
//!   - a flat `Vec<Step>` of `Send` / `Expect` / `Advance`,
//!
//! and we drop the SUT/tier machinery, `or`-branching, `parallel`, media, and
//! chaos steps (see MIGRATION_STATUS.md for the per-feature justification).
//! The trace the reports render is **not** built here — it is projected from
//! the recording layer after the run (`sip_net::to_sip_entries`).

use std::net::SocketAddr;

/// A handle to a declared agent. `Copy` so it can be captured in a `let` and
/// reused across steps without borrowing the [`Scenario`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AgentId(pub usize);

/// A named fake SIP UA bound to a wire address.
#[derive(Clone, Debug)]
pub struct Agent {
    pub name: String,
    pub addr: SocketAddr,
}

/// What an [`Step::Expect`] asserts about the next datagram an agent receives.
#[derive(Clone, Debug)]
pub enum Match {
    /// A request whose method equals this (case-insensitive), e.g. `INVITE`.
    Method(String),
    /// A response whose status code equals this, e.g. `200`.
    Status(u16),
    /// Any well-formed SIP message.
    Any,
}

impl Match {
    pub fn method(m: impl Into<String>) -> Self {
        Match::Method(m.into())
    }
    pub fn status(s: u16) -> Self {
        Match::Status(s)
    }

    /// Human label for reports/assertions.
    pub fn describe(&self) -> String {
        match self {
            Match::Method(m) => m.clone(),
            Match::Status(s) => s.to_string(),
            Match::Any => "<any>".to_string(),
        }
    }
}

/// One scripted action. Executed in order by the driver ([`crate::run`]).
#[derive(Clone, Debug)]
pub enum Step {
    /// `from` sends `raw` bytes addressed to `to`.
    Send {
        from: AgentId,
        to: AgentId,
        raw: Vec<u8>,
    },
    /// `agent` must receive a datagram matching `matcher` within the driver's
    /// per-expect timeout.
    Expect { agent: AgentId, matcher: Match },
    /// Advance virtual time by `ms` (requires a paused tokio runtime; mirrors
    /// the source's 100 ms-chunk `TestClock.adjust`).
    Advance { ms: u64 },
}

/// A scenario: named agents + a flat step list. Built imperatively; carries no
/// execution state.
#[derive(Clone, Debug)]
pub struct Scenario {
    pub name: String,
    pub description: Option<String>,
    pub agents: Vec<Agent>,
    pub steps: Vec<Step>,
}

impl Scenario {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: None,
            agents: Vec::new(),
            steps: Vec::new(),
        }
    }

    /// Human-readable commentary, surfaced in the report header (port of
    /// `.describe(...)`).
    pub fn describe(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Declare an agent at `addr` (e.g. `"127.0.0.1:5060"`). Panics on an
    /// unparseable address — scenarios are static test fixtures, so a bad
    /// address is a defect, not a runtime error.
    pub fn agent(&mut self, name: impl Into<String>, addr: &str) -> AgentId {
        let id = AgentId(self.agents.len());
        self.agents.push(Agent {
            name: name.into(),
            addr: addr.parse().unwrap_or_else(|e| panic!("bad agent addr {addr:?}: {e}")),
        });
        id
    }

    /// `from` sends `raw` to `to`.
    pub fn send(&mut self, from: AgentId, to: AgentId, raw: impl Into<Vec<u8>>) -> &mut Self {
        self.steps.push(Step::Send {
            from,
            to,
            raw: raw.into(),
        });
        self
    }

    /// `agent` expects to receive a message matching `matcher`.
    pub fn expect(&mut self, agent: AgentId, matcher: Match) -> &mut Self {
        self.steps.push(Step::Expect { agent, matcher });
        self
    }

    /// Advance virtual time by `ms`.
    pub fn advance(&mut self, ms: u64) -> &mut Self {
        self.steps.push(Step::Advance { ms });
        self
    }

    pub(crate) fn agent_at(&self, id: AgentId) -> &Agent {
        &self.agents[id.0]
    }
}
