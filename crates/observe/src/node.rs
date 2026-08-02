//! Process node identity — the `node=` field every lifecycle line carries.
//!
//! Read once from the environment (`POD_NAME`, else `HOSTNAME`) unless a runner
//! sets it explicitly: the ordinal an operator correlates a line with is the pod
//! name in kube and the host name everywhere else.

use std::sync::OnceLock;

static NODE: OnceLock<String> = OnceLock::new();

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
pub fn node() -> &'static str {
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
