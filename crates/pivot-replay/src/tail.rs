//! A cell's post-run tail under guard: the work after the interpreter
//! committed its verdict (the settle, the registered gates' stages, the lane
//! oracles) runs so that an unwind still leaves a failed `verdict.json`, never
//! the interpreter's `ok`, before the panic goes on to the exit code.

use std::cell::RefCell;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::path::Path;

use pivot_schema::bundle::{Failure, RunVerdict};

use crate::lanes::panic_text;
use crate::verifications::Gates;

/// What a product's tail keeps of its gates' results as it judges them, and
/// how they are charged to a verdict at each gate's own site.
pub trait Kept: Default {
    /// Charges every kept result to `verdict`.
    fn charge(&self, verdict: &mut RunVerdict);
}

/// The results the tail judged so far, for [`guarded`] to charge if the tail
/// unwinds before it wrote them to the verdict itself.
#[derive(Default)]
pub struct Stages<K: Kept>(RefCell<Held<K>>);

#[derive(Default)]
struct Held<K> {
    kept: K,
    /// Whether the tail's own write of the verdict carries every kept result.
    charged: bool,
}

impl<K: Kept> Stages<K> {
    /// Keeps what `keep` adds to the results, and answers what it answers.
    pub fn keep<T>(&self, keep: impl FnOnce(&mut K) -> T) -> T {
        keep(&mut self.0.borrow_mut().kept)
    }

    /// The verdict on disk now states every kept result: an unwind from here
    /// on charges the panic alone.
    pub fn charged(&self) {
        self.0.borrow_mut().charged = true;
    }
}

/// Runs `tail`, which starts once the interpreter has committed `committed`
/// to `out_dir`. If it unwinds, the verdict on disk (else `committed`) gains
/// the kept results the tail had not yet charged ([`Kept::charge`]) and a
/// `RunUnwound` naming the panic; it is rewritten under `gates` and the
/// unwind resumed.
pub fn guarded<K: Kept, T>(
    out_dir: &Path,
    committed: &RunVerdict,
    stages: &Stages<K>,
    gates: &Gates,
    tail: impl FnOnce() -> T,
) -> T {
    let panic = match catch_unwind(AssertUnwindSafe(tail)) {
        Ok(done) => return done,
        Err(panic) => panic,
    };
    let mut verdict = crate::verdict::read(out_dir).unwrap_or_else(|_| committed.clone());
    let held = stages.0.take();
    if !held.charged {
        held.kept.charge(&mut verdict);
    }
    verdict.fail(Failure::RunUnwound {
        detail: format!("the post-run tail panicked: {}", panic_text(panic.as_ref())),
    });
    if let Err(why) = crate::verdict::write(out_dir, &verdict, gates) {
        crate::diag::line(format_args!(
            "{}: the unwound verdict was not written: {why}",
            out_dir.display()
        ));
    }
    resume_unwind(panic)
}

#[cfg(test)]
#[path = "tail_tests.rs"]
mod tests;
