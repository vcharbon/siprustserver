//! The Call-ID stamp that names a call's seed on the wire, and the registry of
//! what each call was drawn as.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use scenario_harness::actor::{ActorCall, GoalStep};

/// What one call was drawn as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drawn {
    pub seed: u64,
    /// The draw in one line: the shape's choices, then the perturbations.
    pub desc: String,
}

/// The prefix of a stamped Call-ID; the seed's 16 hex digits follow it.
pub const STAMP_PREFIX: &str = "fz-";

/// Unique per process, so a replayed seed never reuses a Call-ID.
fn run_nonce() -> u32 {
    static N: OnceLock<u32> = OnceLock::new();
    *N.get_or_init(|| {
        std::process::id()
            ^ (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0))
    })
}

static SEQ: AtomicU64 = AtomicU64::new(0);

/// The process's draws, by call token.
pub fn drawn() -> &'static Mutex<BTreeMap<String, Drawn>> {
    static D: OnceLock<Mutex<BTreeMap<String, Drawn>>> = OnceLock::new();
    D.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// The seed a stamped Call-ID names (`fz-<16 hex>-…`), wherever it sits in
/// `call_id` (a proxy may prefix it).
pub fn seed_of_call_id(call_id: &str) -> Option<u64> {
    let at = call_id.find(STAMP_PREFIX)? + STAMP_PREFIX.len();
    let hex = call_id.get(at..at + 16)?;
    if call_id.as_bytes().get(at + 16) != Some(&b'-') {
        return None;
    }
    u64::from_str_radix(hex, 16).ok()
}

/// Stamp `seed` into the Call-ID of every INVITE of `call` that states none
/// (`fz-<seed>-<seq>-<nonce>@fuzz.invalid`, unique in the process), and record
/// the call of `token` as drawn `desc`; with `log`, the draw is also one
/// `FUZZ-DRAW <call-id user> seed=… <desc>` line on stderr. An INVITE that
/// states its own Call-ID keeps it, and the log names it.
pub fn seal(call: &mut ActorCall, seed: u64, token: &str, desc: String, log: bool) {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut used = format!("{STAMP_PREFIX}{seed:016x}-{n}-{}@fuzz.invalid", run_nonce());
    for g in call.actors.iter_mut().flat_map(|a| a.goals.iter_mut()) {
        if let GoalStep::Invite { plan: Some(plan), .. } = &mut g.step {
            match &plan.call_id {
                None => plan.call_id = Some(used.clone()),
                Some(stamped) => used = stamped.clone(),
            }
        }
    }
    if log {
        let id = used.split('@').next().unwrap_or(&used);
        eprintln!("FUZZ-DRAW {id} seed={seed:#018x} {desc}");
    }
    if let Ok(mut d) = drawn().lock() {
        d.insert(token.to_string(), Drawn { seed, desc });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stamp_names_its_seed() {
        assert_eq!(seed_of_call_id("fz-00000000000000ff-3-77@fuzz.invalid"), Some(0xff));
        assert_eq!(seed_of_call_id("term1-fz-0123456789abcdef-0-1@x"), Some(0x0123_4567_89ab_cdef));
        assert_eq!(seed_of_call_id("fz-00ff-3-77@fuzz.invalid"), None);
        assert_eq!(seed_of_call_id("plain@host"), None);
    }
}
