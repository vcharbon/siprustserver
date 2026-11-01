//! A router loop that costs time: the turn probe's router-turn hook
//! ([`TurnProbe::router_turn`]) holds a paced node's loop for a set simulated
//! time on each turn, or until its pace changes, so the events behind it queue
//! as they do behind a starved process.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use b2bua::turn_probe::{Hold, TurnMark, TurnProbe};
use tokio::sync::watch;

/// How a node's router loop takes each turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pace {
    /// At once.
    Free,
    /// Each turn holds the loop this long.
    PerTurn(Duration),
    /// The loop takes no turn until the pace changes: a wedged loop.
    Frozen,
}

/// The pace of each node's router loop, by ordinal; a node never set runs
/// [`Pace::Free`].
#[derive(Default)]
pub struct RouterPace {
    nodes: Mutex<HashMap<String, watch::Sender<Pace>>>,
}

impl RouterPace {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Set `node`'s pace. A turn held [`Frozen`](Pace::Frozen) goes on under
    /// the new pace; a turn already paying its cost pays it out.
    pub fn set(&self, node: &str, pace: Pace) {
        let mut nodes = self.nodes.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        match nodes.get(node) {
            Some(tx) => {
                tx.send_replace(pace);
            }
            None => {
                nodes.insert(node.to_string(), watch::channel(pace).0);
            }
        }
    }

    /// The hold of one turn of `node`'s router loop; `None` while it runs free.
    pub fn hold(&self, node: &str) -> Option<Hold> {
        let nodes = self.nodes.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut rx = nodes.get(node)?.subscribe();
        if *rx.borrow_and_update() == Pace::Free {
            return None;
        }
        Some(Box::pin(async move {
            loop {
                let pace = *rx.borrow_and_update();
                match pace {
                    Pace::Free => return,
                    Pace::PerTurn(cost) => {
                        tokio::time::sleep(cost).await;
                        return;
                    }
                    Pace::Frozen => {
                        if rx.changed().await.is_err() {
                            std::future::pending::<()>().await;
                        }
                    }
                }
            }
        }))
    }
}

impl TurnProbe for RouterPace {
    fn reached(&self, _mark: &TurnMark<'_>) -> Option<Hold> {
        None
    }

    fn router_turn(&self, node: &str) -> Option<Hold> {
        self.hold(node)
    }
}
