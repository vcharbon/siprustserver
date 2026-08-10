//! Panic-message capture for the Drop-time artifact writer
//! ([`super::artifact_dump`]). A process-wide panic hook — installed once and
//! CHAINED onto whatever hook was previously installed (the previous hook
//! still runs, so the default stderr report and any test-framework hook are
//! untouched) — stashes the panic message + location in a thread-local slot.
//! A Drop guard on the unwinding thread reads it back via [`take`] while
//! `std::thread::panicking()`.

use std::cell::RefCell;
use std::sync::Once;

thread_local! {
    /// The FIRST panic message raised on this thread since the slot was last
    /// [`take`]n or [`clear`]ed. First-wins: a later panic (e.g. a Drop-time
    /// renderer failing under its own `catch_unwind` while the thread is
    /// already unwinding) must not replace the message of the panic that
    /// actually failed the run.
    static FIRST_PANIC: RefCell<Option<String>> = const { RefCell::new(None) };
}

static INSTALL: Once = Once::new();

/// Install the chained hook. Idempotent — every call after the first is a
/// no-op, so any number of harnesses can call it.
pub(super) fn install() {
    INSTALL.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let message = info
                .payload()
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| info.payload().downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "<non-string panic payload>".to_string());
            let noted = match info.location() {
                Some(l) => format!("{message} (at {}:{}:{})", l.file(), l.line(), l.column()),
                None => message,
            };
            FIRST_PANIC.with(|slot| {
                let mut slot = slot.borrow_mut();
                if slot.is_none() {
                    *slot = Some(noted);
                }
            });
            previous(info);
        }));
    });
}

/// Take (and clear) the message the hook captured on this thread.
pub(super) fn take() -> Option<String> {
    FIRST_PANIC.with(|slot| slot.borrow_mut().take())
}

/// Drop any stale note on this thread — a guard whose gate was off never
/// [`take`]s, so a fresh harness clears the slot before its run can panic.
pub(super) fn clear() {
    FIRST_PANIC.with(|slot| *slot.borrow_mut() = None);
}
