//! Folded-stack lines from a profiler report: the input format of a flamegraph
//! renderer.

use pprof::Frames;

/// One folded line per sampled stack: `thread;outermost;…;innermost count`.
///
/// Frames are stored innermost first, and the symbols of one frame (inlined
/// calls) innermost first too; both are reversed here.
pub fn folded_line(frames: &Frames, count: isize) -> String {
    let mut names = vec![frames.thread_name_or_id()];
    for frame in frames.frames.iter().rev() {
        for symbol in frame.iter().rev() {
            names.push(symbol.to_string());
        }
    }
    format!("{} {count}", names.join(";"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pprof::Symbol;
    use std::time::SystemTime;

    fn symbol(name: &str) -> Symbol {
        Symbol { name: Some(name.as_bytes().to_vec()), addr: None, lineno: None, filename: None }
    }

    #[test]
    fn a_stack_folds_outermost_first_with_inlined_calls_in_call_order() {
        let frames = Frames {
            frames: vec![vec![symbol("leaf"), symbol("inlined_in_mid")], vec![symbol("main")]],
            thread_name: "worker".to_string(),
            thread_id: 7,
            sample_timestamp: SystemTime::UNIX_EPOCH,
        };
        assert_eq!(folded_line(&frames, 3), "worker;main;inlined_in_mid;leaf 3");
    }

    #[test]
    fn an_unnamed_thread_folds_under_its_id() {
        let frames = Frames {
            frames: vec![vec![symbol("main")]],
            thread_name: String::new(),
            thread_id: 42,
            sample_timestamp: SystemTime::UNIX_EPOCH,
        };
        assert_eq!(folded_line(&frames, 1), "42;main 1");
    }
}
