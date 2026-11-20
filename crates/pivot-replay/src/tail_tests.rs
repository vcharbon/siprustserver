use std::path::PathBuf;

use pivot_schema::bundle::VerdictStatus;

use super::*;

const SITE: &str = "post-run drain";

/// A product's kept results: one breach per stage, charged as one failure.
#[derive(Default)]
struct Breaches(Vec<String>);

impl Kept for Breaches {
    fn charge(&self, verdict: &mut RunVerdict) {
        if self.0.is_empty() {
            return;
        }
        verdict.fail(Failure::CheckFailed {
            site: SITE.into(),
            field: "sut.drained".into(),
            op: "drained".into(),
            expected: "nothing held".into(),
            observed: self.0.join("; "),
        });
    }
}

fn green() -> RunVerdict {
    RunVerdict::ok("c", "lane")
}

fn committed_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pivot-tail-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    crate::verdict::write(&dir, &green(), &Gates::NONE).unwrap();
    dir
}

fn unwind(dir: &Path, body: impl FnOnce(&Stages<Breaches>, &Path)) -> String {
    let stages = Stages::default();
    let resumed = catch_unwind(AssertUnwindSafe(|| {
        guarded(dir, &green(), &stages, &Gates::NONE, || {
            body(&stages, dir);
            panic!("tail fault")
        })
    }))
    .expect_err("the unwind goes on");
    panic_text(resumed.as_ref())
}

#[test]
fn an_unwound_tail_charges_what_it_kept_and_the_unwind_apart() {
    let dir = committed_dir("unwound");
    let text = unwind(&dir, |stages, _| stages.keep(|kept| kept.0.push("1 hold left".into())));
    assert_eq!(text, "tail fault", "the panic resumes with its own payload");
    let verdict = crate::verdict::read(&dir).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(verdict.status, VerdictStatus::Failed);
    assert!(
        matches!(&verdict.failures[..], [
            Failure::CheckFailed { site, observed, .. },
            Failure::RunUnwound { detail },
        ] if site == SITE
            && observed == "1 hold left"
            && detail == "the post-run tail panicked: tail fault"),
        "{:?}",
        verdict.failures
    );
}

#[test]
fn results_the_tail_already_charged_are_not_charged_twice() {
    let dir = committed_dir("charged");
    unwind(&dir, |stages, dir| {
        stages.keep(|kept| kept.0.push("1 hold left".into()));
        let mut verdict = green();
        stages.keep(|kept| kept.charge(&mut verdict));
        crate::verdict::write(dir, &verdict, &Gates::NONE).unwrap();
        stages.charged();
    });
    let verdict = crate::verdict::read(&dir).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(
        matches!(&verdict.failures[..], [
            Failure::CheckFailed { observed, .. },
            Failure::RunUnwound { .. },
        ] if observed == "1 hold left"),
        "{:?}",
        verdict.failures
    );
}

#[test]
fn a_returning_tail_leaves_the_verdict_alone() {
    let dir = committed_dir("returning");
    let before = std::fs::read(dir.join(crate::verdict::FILE)).unwrap();
    let stages = Stages::<Breaches>::default();
    let done = guarded(&dir, &green(), &stages, &Gates::NONE, || {
        stages.keep(|kept| kept.0.push("1 hold left".into()));
        7
    });
    assert_eq!(done, 7, "the tail's own result is handed back");
    assert_eq!(std::fs::read(dir.join(crate::verdict::FILE)).unwrap(), before, "byte-identical");
    std::fs::remove_dir_all(&dir).unwrap();
}
