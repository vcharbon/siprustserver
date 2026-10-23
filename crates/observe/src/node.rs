//! Node identity — the `node=` field every lifecycle line carries and the node
//! a per-call span names.
//!
//! The process identity is read once from the environment (`POD_NAME`, else
//! `HOSTNAME`) unless a runner sets it explicitly: the ordinal an operator
//! correlates a line with is the pod name in kube and the host name everywhere
//! else.
//!
//! Code may serve a node other than the process's own — several nodes sharing
//! one process in a simulation. A [`NodeScope`] names that node for a future
//! or a synchronous call; every task spawned through [`spawn_keeping_node`] or
//! wrapped in [`keep_node`] keeps the node of the code that created it. Outside
//! any scope the node is the process identity.

use std::future::Future;
use std::sync::{Arc, OnceLock};

use tokio::task::futures::TaskLocalFuture;
use tokio::task::JoinHandle;

static NODE: OnceLock<String> = OnceLock::new();

tokio::task_local! {
    /// The node the running code serves; `None` is the process identity.
    static SERVING: Option<Arc<str>>;
}

/// Env vars consulted, in order, when nothing was set explicitly.
const ENV_CANDIDATES: [&str; 2] = ["POD_NAME", "HOSTNAME"];

/// Value reported when neither the environment nor the runner names this node.
const UNKNOWN: &str = "unknown";

/// Pin this process's node identity. The first call wins; later calls are inert
/// (identity must not change mid-process — an operator reads it as a key).
pub fn set_node_identity(id: impl Into<String>) {
    let _ = NODE.set(id.into());
}

/// This process's node identity, resolved from the environment on first read.
fn process_node() -> &'static str {
    NODE.get_or_init(|| {
        for var in ENV_CANDIDATES {
            if let Ok(v) = std::env::var(var) {
                let v = v.trim();
                if !v.is_empty() {
                    return v.to_string();
                }
            }
        }
        UNKNOWN.to_string()
    })
}

/// The node the running code serves: its [`NodeScope`], else the process
/// identity.
pub fn node() -> String {
    with_node(str::to_string)
}

/// Read the node the running code serves without allocating.
pub fn with_node<R>(f: impl FnOnce(&str) -> R) -> R {
    match NodeScope::current().0 {
        Some(node) => f(&node),
        None => f(process_node()),
    }
}

/// The node some code serves, held to run code as that node later — from a
/// task that serves another, or none.
#[derive(Debug, Clone, Default)]
pub struct NodeScope(Option<Arc<str>>);

impl NodeScope {
    /// The scope of the running code; the process identity outside one.
    pub fn current() -> Self {
        Self(SERVING.try_with(Clone::clone).ok().flatten())
    }

    /// The scope serving `node`.
    pub fn named(node: impl Into<Arc<str>>) -> Self {
        Self(Some(node.into()))
    }

    /// Run `f` as this scope's node; what `f` spawns through
    /// [`spawn_keeping_node`] keeps it.
    pub fn enter<R>(&self, f: impl FnOnce() -> R) -> R {
        SERVING.sync_scope(self.0.clone(), f)
    }

    /// `fut` running as this scope's node.
    pub fn scope<F: Future>(&self, fut: F) -> TaskLocalFuture<Option<Arc<str>>, F> {
        SERVING.scope(self.0.clone(), fut)
    }
}

/// The scope's node, as [`node`] reads it inside the scope.
impl std::fmt::Display for NodeScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Some(node) => f.write_str(node),
            None => f.write_str(process_node()),
        }
    }
}

/// `fut` serving the node of the code that calls this.
pub fn keep_node<F: Future>(fut: F) -> TaskLocalFuture<Option<Arc<str>>, F> {
    NodeScope::current().scope(fut)
}

/// `tokio::spawn` a task that serves the node of the code that spawns it.
pub fn spawn_keeping_node<F>(fut: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    tokio::spawn(keep_node(fut))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn outside_a_scope_the_node_is_the_process_identity() {
        assert_eq!(node(), process_node());
    }

    #[tokio::test]
    async fn a_scope_names_its_node_and_a_spawned_task_keeps_it() {
        let (inside, spawned) = NodeScope::named("n1#g2")
            .scope(async {
                let spawned = spawn_keeping_node(async { node() }).await.expect("task completes");
                (node(), spawned)
            })
            .await;
        assert_eq!(inside, "n1#g2");
        assert_eq!(spawned, "n1#g2", "a task spawned inside the scope serves the same node");
        assert_eq!(node(), process_node(), "the scope ends with its future");
    }

    #[tokio::test]
    async fn an_entered_scope_reaches_what_it_spawns() {
        let task = NodeScope::named("n2#g1").enter(|| spawn_keeping_node(async { node() }));
        assert_eq!(task.await.expect("task completes"), "n2#g1");
    }

    #[tokio::test]
    async fn a_held_scope_runs_code_as_its_node_from_anywhere() {
        let held = NodeScope::named("n3#g1").enter(NodeScope::current);
        assert_eq!(held.enter(node), "n3#g1");
        assert_eq!(held.to_string(), "n3#g1", "a scope displays its node");
        assert_eq!(NodeScope::default().enter(node), process_node());
        assert_eq!(NodeScope::default().to_string(), process_node());
    }

    #[tokio::test]
    async fn a_plain_tokio_task_does_not_inherit_the_scope() {
        let plain = NodeScope::named("n4#g1")
            .scope(async { tokio::spawn(async { node() }).await.expect("task completes") })
            .await;
        assert_eq!(plain, process_node(), "only `spawn_keeping_node` and `keep_node` carry it");
    }
}
