use std::io::Write;

use super::*;

/// A writer keeping every line written to it.
#[derive(Clone, Default)]
struct Kept(Arc<Mutex<Vec<u8>>>);

impl Write for Kept {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Kept {
    /// The `ask` of every inject line written, in order.
    fn asks(&self) -> Vec<String> {
        let bytes = self.0.lock().unwrap_or_else(|p| p.into_inner()).clone();
        String::from_utf8(bytes)
            .expect("utf-8 lines")
            .lines()
            .map(|line| {
                let value: serde_json::Value = serde_json::from_str(line).expect("a JSON line");
                value["inject"]["ask"].as_str().expect("an ask id").to_string()
            })
            .collect()
    }
}

/// The inject lines of a batch serving its protocol lines, and what it wrote.
fn speaking() -> (Arc<Injects>, Kept) {
    let injects = Arc::new(Injects::default());
    let kept = Kept::default();
    let out: Out = Arc::new(Mutex::new(Box::new(kept.clone())));
    injects.speak_on(out);
    (injects, kept)
}

/// The request of `node`, `operation:restart`.
fn request(node: &str) -> InjectRequest {
    InjectRequest { node: node.into(), action: "operation:restart".into(), target: None }
}

/// `(spec, node)` owned.
fn key(spec: &str, node: &str) -> (String, String) {
    (spec.to_string(), node.to_string())
}

/// The `injected` line echoing `ask` of `node` of the cell at `spec`.
fn injected(ask: &str, spec: &str, node: &str) -> String {
    serde_json::json!({ "injected": { "ask": ask, "spec": spec, "node": node } }).to_string()
}

/// The `refused` line echoing `ask` of `node` of the cell at `spec`.
fn refused(ask: &str, spec: &str, node: &str, reason: &str) -> String {
    serde_json::json!({ "refused": { "ask": ask, "spec": spec, "node": node, "reason": reason } })
        .to_string()
}

/// **A run's injects end with the run**: once the run that asked `i1` of
/// `p` dropped its injector (it timed out and ended), `i1` of `p` is no longer
/// asked, a late answer for it routes nowhere, and the injects of other runs
/// stay asked.
#[tokio::test]
async fn an_ended_run_s_injects_are_no_longer_asked() {
    let (injects, kept) = speaking();
    let ended = injects.injector("/cells/p.spec.json").expect("an injector");
    let other = injects.injector("/cells/q.spec.json").expect("an injector");
    let abandoned = ended.inject(request("i1"));
    let waiting = other.inject(request("i1"));
    drop(abandoned);
    drop(ended);

    assert_eq!(injects.asked(), [key("/cells/q.spec.json", "i1")]);
    let asks = kept.asks();
    injects.answer(&injected(&asks[0], "/cells/p.spec.json", "i1"));
    injects.answer(&injected(&asks[1], "/cells/q.spec.json", "i1"));
    assert_eq!(waiting.await, Ok(()));
    assert_eq!(injects.asked(), []);
}

/// **An answer reaches only the ask whose id it echoes**: a run of `p` asked
/// `i1`, then a re-run of `p` asked `i1` again while the first was still
/// asked; the late answer echoing the first ask's id resolves the first ask,
/// never the re-run's, which stays asked until its own answer comes.
#[tokio::test]
async fn a_late_answer_for_an_earlier_ask_does_not_resolve_a_re_run_s_ask() {
    let (injects, kept) = speaking();
    let earlier = injects.injector("/cells/p.spec.json").expect("an injector");
    let rerun = injects.injector("/cells/p.spec.json").expect("an injector");
    let superseded = earlier.inject(request("i1"));
    let waiting = rerun.inject(request("i1"));
    let asks = kept.asks();
    assert_ne!(asks[0], asks[1], "each ask its own id");

    injects.answer(&injected(&asks[0], "/cells/p.spec.json", "i1"));
    assert_eq!(injects.asked(), [key("/cells/p.spec.json", "i1")], "the re-run's ask still asked");
    assert_eq!(superseded.await, Ok(()));
    injects.answer(&refused(&asks[1], "/cells/p.spec.json", "i1", "exit 1"));
    assert_eq!(waiting.await, Err("exit 1".to_string()));
}

/// **A run withdraws only its own injects**: a run of `p` ending after a later
/// run of `p` asked the same node leaves the later one's inject asked, an
/// answer echoing the ended run's ask routes nowhere, and the later run's own
/// answer reaches it.
#[tokio::test]
async fn an_ended_run_leaves_a_later_run_s_inject_of_the_same_cell_asked() {
    let (injects, kept) = speaking();
    let earlier = injects.injector("/cells/p.spec.json").expect("an injector");
    let later = injects.injector("/cells/p.spec.json").expect("an injector");
    let _superseded = earlier.inject(request("i1"));
    let waiting = later.inject(request("i1"));
    drop(earlier);

    assert_eq!(injects.asked(), [key("/cells/p.spec.json", "i1")]);
    let asks = kept.asks();
    injects.answer(&injected(&asks[0], "/cells/p.spec.json", "i1"));
    assert_eq!(
        injects.asked(),
        [key("/cells/p.spec.json", "i1")],
        "the ended run's answer routes nowhere"
    );
    injects.answer(&refused(&asks[1], "/cells/p.spec.json", "i1", "exit 1"));
    assert_eq!(waiting.await, Err("exit 1".to_string()));
}

/// **An answer echoes the whole ask**: an answer bearing an ask's id but
/// another run-spec or node resolves nothing.
#[tokio::test]
async fn an_answer_naming_another_node_than_its_ask_s_resolves_nothing() {
    let (injects, kept) = speaking();
    let run = injects.injector("/cells/p.spec.json").expect("an injector");
    let waiting = run.inject(request("i1"));
    let ask = kept.asks().remove(0);

    injects.answer(&injected(&ask, "/cells/p.spec.json", "i2"));
    injects.answer(&injected(&ask, "/cells/q.spec.json", "i1"));
    assert_eq!(injects.asked(), [key("/cells/p.spec.json", "i1")]);
    injects.answer(&injected(&ask, "/cells/p.spec.json", "i1"));
    assert_eq!(waiting.await, Ok(()));
}

/// **The waiting line names what the harness owes**: each inject asked and not
/// answered, its run-spec by file name, and none once it was answered.
#[tokio::test]
async fn the_awaited_injects_are_named_by_run_spec_file_and_node() {
    let (injects, kept) = speaking();
    assert_eq!(injects.awaited(), "no inject asked");
    let p = injects.injector("/cells/p.spec.json").expect("an injector");
    let q = injects.injector("/cells/q.spec.json").expect("an injector");
    let _p1 = p.inject(request("i1"));
    let _q2 = q.inject(request("i2"));

    assert_eq!(injects.awaited(), "2 inject(s) asked: p.spec.json i1, q.spec.json i2");
    injects.answer(&injected(&kept.asks()[0], "/cells/p.spec.json", "i1"));
    assert_eq!(injects.awaited(), "1 inject(s) asked: q.spec.json i2");
}
