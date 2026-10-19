//! The proxy's boot configuration read from the process: its environment and
//! the namespace of the pod it runs in.

use std::cell::Cell;
use std::env;

use super::{refusal, ProxyConfig};

/// Where a pod's service account states its namespace.
const NAMESPACE_FILE: &str = "/var/run/secrets/kubernetes.io/serviceaccount/namespace";

impl ProxyConfig {
    /// Every boot variable of the process environment. `Err` is the boot
    /// refusal, naming every problem: a variable that is not UTF-8 among
    /// them.
    pub fn from_env() -> Result<Self, String> {
        let not_unicode = Cell::new(Vec::new());
        let get = |key: &str| match env::var(key) {
            Ok(v) => Some(v),
            Err(env::VarError::NotPresent) => None,
            Err(env::VarError::NotUnicode(_)) => {
                let mut keys = not_unicode.take();
                keys.push(format!("{key} is not UTF-8"));
                not_unicode.set(keys);
                None
            }
        };
        let read = Self::from_lookup(&get, &pod_namespace);
        let mut violations = not_unicode.take();
        match read {
            Ok(cfg) if violations.is_empty() => Ok(cfg),
            Ok(_) => Err(refusal(&violations)),
            Err(more) => {
                violations.extend(more);
                Err(refusal(&violations))
            }
        }
    }
}

/// The namespace of the pod this proxy runs in, from its service account.
fn pod_namespace() -> Option<String> {
    std::fs::read_to_string(NAMESPACE_FILE).ok().map(|ns| ns.trim().to_string())
}
