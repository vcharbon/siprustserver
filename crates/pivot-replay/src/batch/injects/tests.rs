use std::io::Write;

use super::*;

/// A writer that keeps nothing.
struct Silent;

impl Write for Silent {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The inject lines of a batch serving its protocol lines.
fn speaking() -> Arc<Injects> {
    let injects = Arc::new(Injects::default());
    let out: Out = Arc::new(Mutex::new(Box::new(Silent)));
    injects.speak_on(out);
    injects
}

/// The request of `node`, `operation:restart`.
fn request(node: &str) -> InjectRequest {
    InjectRequest { node: node.into(), action: "operation:restart".into(), target: None }
}

/// `(spec, node)` owned.
fn key(spec: &str, node: &str) -> (String, String) {
    (spec.to_string(), node.to_string())
}

/// **A run's injects end with the run**: once the run that asked `i1` of
/// `p` dropped its injector (it timed out and ended), `i1` of `p` is no longer
/// asked, a late answer for it routes nowhere, and the injects of other runs
/// stay asked.
#[tokio::test]
async fn an_ended_run_s_injects_are_no_longer_asked() {
    let injects = speaking();
    let ended = injects.injector("/cells/p.spec.json").expect("an injector");
    let other = injects.injector("/cells/q.spec.json").expect("an injector");
    let abandoned = ended.inject(request("i1"));
    let waiting = other.inject(request("i1"));
    drop(abandoned);
    drop(ended);

    assert_eq!(injects.asked(), [key("/cells/q.spec.json", "i1")]);
    injects.answer(r#"{"injected":{"spec":"/cells/p.spec.json","node":"i1"}}"#);
    injects.answer(r#"{"injected":{"spec":"/cells/q.spec.json","node":"i1"}}"#);
    assert_eq!(waiting.await, Ok(()));
    assert_eq!(injects.asked(), []);
}

/// **A run withdraws only its own injects**: a run of `p` ending after a later
/// run of `p` asked the same node leaves the later one's inject asked, and
/// the answer reaches the later run.
#[tokio::test]
async fn an_ended_run_leaves_a_later_run_s_inject_of_the_same_cell_asked() {
    let injects = speaking();
    let earlier = injects.injector("/cells/p.spec.json").expect("an injector");
    let later = injects.injector("/cells/p.spec.json").expect("an injector");
    let _superseded = earlier.inject(request("i1"));
    let waiting = later.inject(request("i1"));
    drop(earlier);

    assert_eq!(injects.asked(), [key("/cells/p.spec.json", "i1")]);
    injects.answer(r#"{"refused":{"spec":"/cells/p.spec.json","node":"i1","reason":"exit 1"}}"#);
    assert_eq!(waiting.await, Err("exit 1".to_string()));
}

/// **The waiting line names what the harness owes**: each inject asked and not
/// answered, its run-spec by file name, and none once it was answered.
#[tokio::test]
async fn the_awaited_injects_are_named_by_run_spec_file_and_node() {
    let injects = speaking();
    assert_eq!(injects.awaited(), "no inject asked");
    let p = injects.injector("/cells/p.spec.json").expect("an injector");
    let q = injects.injector("/cells/q.spec.json").expect("an injector");
    let _p1 = p.inject(request("i1"));
    let _q2 = q.inject(request("i2"));

    assert_eq!(injects.awaited(), "2 inject(s) asked: p.spec.json i1, q.spec.json i2");
    injects.answer(r#"{"injected":{"spec":"/cells/p.spec.json","node":"i1"}}"#);
    assert_eq!(injects.awaited(), "1 inject(s) asked: q.spec.json i2");
}
