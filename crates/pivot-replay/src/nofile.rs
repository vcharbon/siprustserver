//! The process's open-descriptor limit: a batch holds several sockets per cell
//! in flight, so it runs with its soft `RLIMIT_NOFILE` raised to the hard one.

use rustix::process::{getrlimit, setrlimit, Resource, Rlimit};

/// Raises the soft descriptor limit to the hard one and states the limit the
/// process runs with: `"<n>"` or `"<n> (raised from <m>)"`; where the raise is
/// refused, `"<m> (not raised: <why>)"` as the error.
pub fn raise() -> Result<String, String> {
    let limit = getrlimit(Resource::Nofile);
    if limit.current == limit.maximum {
        return Ok(shown(limit.current));
    }
    let raised = Rlimit { current: limit.maximum, maximum: limit.maximum };
    match setrlimit(Resource::Nofile, raised) {
        Ok(()) => Ok(format!("{} (raised from {})", shown(limit.maximum), shown(limit.current))),
        Err(e) => Err(format!("{} (not raised: {e})", shown(limit.current))),
    }
}

/// A limit as the line states it; `None` is no limit.
fn shown(limit: Option<u64>) -> String {
    limit.map_or_else(|| "unlimited".to_string(), |n| n.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A process whose soft limit sits below its hard one (lowering is always
    /// permitted; nextest runs this test in a process of its own) raises it
    /// to the hard one and states both.
    #[test]
    fn a_lowered_soft_limit_is_raised_to_the_hard_one() {
        let hard = getrlimit(Resource::Nofile).maximum;
        let low = hard.map_or(1024, |hard| 1024.min(hard / 2));
        setrlimit(Resource::Nofile, Rlimit { current: Some(low), maximum: hard })
            .expect("a process may lower its soft limit");

        let stated = raise().expect("a process may raise its soft limit to its hard one");
        assert_eq!(stated, format!("{} (raised from {low})", shown(hard)));
        assert_eq!(getrlimit(Resource::Nofile).current, hard);
    }
}
