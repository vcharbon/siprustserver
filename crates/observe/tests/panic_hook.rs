//! The installed process panic hook: over a capturing previous hook, two
//! panics at one site inside the report interval forward once, and a panic
//! at that site past the interval forwards again. A binary of its own: the
//! panic hook is process-global (ADR-0030 X2).

use std::panic::catch_unwind;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use observe::panic_report::install_with;

/// Panics the previous hook received.
static FORWARDED: AtomicUsize = AtomicUsize::new(0);
/// The fake clock: milliseconds past `BASE`.
static ELAPSED_MS: AtomicU64 = AtomicU64::new(0);
static BASE: OnceLock<Instant> = OnceLock::new();

fn fake_now() -> Instant {
    *BASE.get_or_init(Instant::now) + Duration::from_millis(ELAPSED_MS.load(Ordering::SeqCst))
}

/// One panic site, reached by every call.
fn panic_here() {
    panic!("probe: the same site");
}

#[test]
fn the_installed_hook_forwards_a_site_once_per_interval() {
    std::panic::set_hook(Box::new(|_| {
        FORWARDED.fetch_add(1, Ordering::SeqCst);
    }));
    install_with(Duration::from_secs(60), fake_now);

    assert!(catch_unwind(panic_here).is_err());
    assert!(catch_unwind(panic_here).is_err());
    assert_eq!(FORWARDED.load(Ordering::SeqCst), 1, "a repeat inside the interval is counted");

    ELAPSED_MS.store(60_000, Ordering::SeqCst);
    assert!(catch_unwind(panic_here).is_err());
    assert_eq!(FORWARDED.load(Ordering::SeqCst), 2, "past the interval the site reports again");

    let _ = std::panic::take_hook();
}
