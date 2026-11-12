//! A cell's diagnostic lines: written to the process's stderr, and kept for
//! the cell that wrote them where one is capturing. A batch runs every cell on
//! a thread of its own inside one process, so its stderr interleaves them all;
//! the lines a cell keeps travel on its own protocol line instead.

use std::cell::RefCell;
use std::fmt::Display;

thread_local! {
    static KEPT: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Writes `text` as one line of stderr, and keeps it for the capturing cell.
pub fn line(text: impl Display) {
    let text = text.to_string();
    eprintln!("{text}");
    KEPT.with(|kept| {
        if let Some(kept) = kept.borrow_mut().as_mut() {
            kept.push_str(&text);
            kept.push('\n');
        }
    });
}

/// Runs `cell` on this thread, answering what it returned and every line it
/// wrote through [`line()`]. A panic passing through ends the capture with it.
pub fn captured<R>(cell: impl FnOnce() -> R) -> (R, String) {
    struct Ends;
    impl Drop for Ends {
        fn drop(&mut self) {
            KEPT.with(|kept| kept.borrow_mut().take());
        }
    }
    KEPT.with(|kept| *kept.borrow_mut() = Some(String::new()));
    let ends = Ends;
    let value = cell();
    let kept = KEPT.with(|kept| kept.borrow_mut().take()).unwrap_or_default();
    drop(ends);
    (value, kept)
}

#[cfg(test)]
#[path = "diag_tests.rs"]
mod tests;
