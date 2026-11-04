//! The branch memo's variables: its bounds, the mean ring and INVITEs per
//! call they are split on, and the INVITE entry's cap.

use sip_proxy::cancel_lru::{invite_entry_ttl_ms, MemoBounds};

use super::reader::EnvReader;
use super::MAX_MS;

/// `PROXY_BRANCH_MEMO_INVITES_PER_CALL`, thousandths: the INVITEs a call
/// sends across the pair on average, a decimal of at most three places from 2
/// (a basic call's) to 100.
fn invites_per_call_milli(env: &mut EnvReader) -> u64 {
    let key = "PROXY_BRANCH_MEMO_INVITES_PER_CALL";
    let Some(value) = env.present(key) else { return 2_000 };
    match thousandths(&value) {
        Some(milli @ 2_000..=100_000) => milli,
        _ => {
            env.refuse(format!("{key} must be a decimal from 2 to 100, got {value:?}"));
            2_000
        }
    }
}

/// `value`, a decimal of at most three places, in thousandths.
fn thousandths(value: &str) -> Option<u64> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty() || fraction.len() > 3 || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let padded = format!("{fraction:0<3}");
    whole.parse::<u64>().ok()?.checked_mul(1_000)?.checked_add(padded.parse::<u64>().ok()?)
}

/// The branch memo's entry cap and byte budget, the mean ring they are
/// split on, and the INVITE entry's cap, derived from the downstream INVITE
/// transaction bound `PROXY_INVITE_TXN_TIMEOUT_SEC` (the workers' bound).
pub(super) fn branch_memo(env: &mut EnvReader) -> MemoBounds {
    let d = MemoBounds::default();
    let size = |env: &mut EnvReader, key: &str, default: usize| {
        let default = u64::try_from(default).unwrap_or(u64::MAX);
        usize::try_from(env.int(key, default, 1, u64::MAX)).unwrap_or(usize::MAX)
    };
    let bound_sec = env.int(
        "PROXY_INVITE_TXN_TIMEOUT_SEC",
        sip_txn::timers::INVITE_INITIAL_TIMEOUT / 1000,
        33,
        600,
    );
    MemoBounds {
        max_entries: size(env, "PROXY_BRANCH_MEMO_MAX_ENTRIES", d.max_entries),
        max_bytes: size(env, "PROXY_BRANCH_MEMO_MAX_BYTES", d.max_bytes),
        mean_ring_ms: env.int("PROXY_BRANCH_MEMO_MEAN_RING_MS", d.mean_ring_ms, 1, MAX_MS),
        invite_ttl_ms: invite_entry_ttl_ms(bound_sec),
        invites_per_call_milli: invites_per_call_milli(env),
        ..d
    }
}
