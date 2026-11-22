//! The UAs a harness bound, past the scenario's last step. While a fixture
//! waits for its SUT to settle, each keeps serving its transactions, as a
//! stack does whether or not its user still reads: a copy of a request it
//! answered draws that final again until Timer J (RFC 3261 §17.2.2), a repeat
//! of a non-2xx final it ACKed draws the ACK again until Timer D
//! (§17.1.1.2), and its own requests, finals and reliable provisionals go
//! again until answered (§17.1, §17.2.1, RFC 3262 §3). At the end of the run
//! its dialog table judges what it left unread.

use std::time::Duration;

use tokio::time::Instant;

use super::Harness;
use crate::agent::weak_agent::WeakAgent;
use crate::agent::Agent;

/// How often the wait re-checks its condition and serves the parties.
const POLL: Duration = Duration::from_millis(5);

/// The longest the wait runs on a fabric that loses nothing.
const RELIABLE_BOUND: Duration = Duration::from_secs(1);

/// The longest the wait runs on a fabric that loses datagrams: past the
/// SUT's own 64·T1 give-up on a transaction it started before the wait, by
/// two T1.
const LOSSY_BOUND: Duration = Duration::from_millis(66 * 500);

/// How long no datagram reaches a party before a fabric that loses
/// datagrams counts as quiet: past T2, the longest interval between two
/// sends of a final or a non-INVITE request (RFC 3261 §17.1.2.2, §17.2.1),
/// by one T1. A SUT transaction outliving its call (a final whose ACK was
/// lost) is answered within it.
const LOSSY_QUIET: Duration = Duration::from_millis(4_500);

impl Harness {
    /// Hold `agent` for the parties' service and the end-of-run sighting of
    /// what it left unread.
    pub(crate) fn keep_bound(&self, agent: &Agent) {
        self.bound.borrow_mut().push(agent.downgrade());
    }

    /// Let every UA's dialog table judge what it still has queued, left
    /// unread, so a request no step pulled still meets its table.
    pub(super) fn judge_unread(&self) {
        for agent in self.bound.borrow().iter().filter_map(WeakAgent::upgrade) {
            agent.judge_unread();
        }
    }

    /// Serve every bound UA's transactions until `cond` holds, re-checking
    /// every 5 ms: within 1 s on a fabric that loses nothing, where a `cond`
    /// that holds at once serves nothing; on one that runs a loss model,
    /// until `cond` holds and no datagram reached a party for T2 and a T1,
    /// at most past the SUT's own 64·T1 give-up. Bounded, so a `cond` that
    /// never holds falls through to the caller's assertions.
    pub async fn serve_parties_until(&self, cond: impl Fn() -> bool) {
        let start = Instant::now();
        let bound = if self.lossy { LOSSY_BOUND } else { RELIABLE_BOUND };
        let mut heard = start;
        loop {
            let now = Instant::now();
            let quiet = !self.lossy || now - heard >= LOSSY_QUIET;
            if (cond() && quiet) || now >= start + bound {
                return;
            }
            if self.serve_parties().await {
                heard = Instant::now();
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// One pass over every bound UA: sight what it has queued, which answers
    /// a copy and re-ACKs a repeated final, and send what its timers owe.
    /// What its user would read is held back for a later receive. Whether a
    /// datagram had reached one.
    async fn serve_parties(&self) -> bool {
        let agents: Vec<Agent> =
            self.bound.borrow().iter().filter_map(WeakAgent::upgrade).collect();
        let mut heard = false;
        for agent in agents {
            heard |= agent.ep.queue_depth() > 0;
            agent.sight_queued().await;
            agent.send_due().await;
        }
        heard
    }
}
