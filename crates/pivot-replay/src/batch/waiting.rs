//! What the batch waits for, stated on stderr every [`EVERY`] while one wait
//! lasts: the harness's stdin and the cells still running, then each step of
//! the batch's end with the bound it holds to. A batch that looks idle names
//! what it is waiting on.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio::time::Instant;

/// How often a lasting wait is stated.
pub const EVERY: Duration = Duration::from_secs(30);

/// What a wait states, read each time it is stated.
type What = Box<dyn Fn() -> String + Send + Sync>;

/// The wait under way: what it states, since when, when it was last stated.
struct Wait {
    what: What,
    since: Instant,
    said: Instant,
}

/// The batch's current wait, and the task stating it.
pub struct Waiting {
    now: Arc<Mutex<Option<Wait>>>,
    ticker: JoinHandle<()>,
    /// Every step stated, in order.
    steps: Mutex<Vec<String>>,
}

impl Waiting {
    /// Starts stating on the current runtime: each wait once it lasted
    /// `every`, then every `every`, each line handed to `say`.
    pub fn start(every: Duration, say: impl Fn(&str) + Send + 'static) -> Waiting {
        let now: Arc<Mutex<Option<Wait>>> = Arc::default();
        let read = now.clone();
        let poll = (every / 30).max(Duration::from_millis(1));
        let ticker = tokio::spawn(async move {
            loop {
                tokio::time::sleep(poll).await;
                let line = {
                    let mut now = read.lock().unwrap_or_else(|p| p.into_inner());
                    match now.as_mut() {
                        Some(wait) if wait.said.elapsed() >= every => {
                            wait.said = Instant::now();
                            Some(format!(
                                "replay batch: waiting {} s for {}",
                                wait.since.elapsed().as_secs(),
                                (wait.what)()
                            ))
                        }
                        _ => None,
                    }
                };
                if let Some(line) = line {
                    say(&line);
                }
            }
        });
        Waiting { now, ticker, steps: Mutex::default() }
    }

    /// The batch waits, from now on, for what `what` states.
    pub fn on(&self, what: impl Fn() -> String + Send + Sync + 'static) {
        let at = Instant::now();
        *self.now.lock().unwrap_or_else(|p| p.into_inner()) =
            Some(Wait { what: Box::new(what), since: at, said: at });
    }

    /// The batch waits, from now on, for `step`.
    pub fn step(&self, step: impl Into<String>) {
        let step = step.into();
        self.steps.lock().unwrap_or_else(|p| p.into_inner()).push(step.clone());
        self.on(move || step.clone());
    }

    /// Every step stated so far, in order.
    pub fn steps(&self) -> Vec<String> {
        self.steps.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// The batch waits for nothing.
    pub fn done(&self) {
        *self.now.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        self.ticker.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A wait is stated once it lasted the period, then every period, each
    /// time as it reads then; a short wait and no wait are never stated.
    #[tokio::test(start_paused = true)]
    async fn a_lasting_wait_is_stated_every_period_and_a_short_one_never() {
        let said = Arc::new(Mutex::new(Vec::<String>::new()));
        let into = said.clone();
        let waiting = Waiting::start(EVERY, move |line| into.lock().unwrap().push(line.into()));
        waiting.step("the relay's last exchange (at most 2 s)");
        tokio::time::sleep(Duration::from_secs(2)).await;
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = count.clone();
        waiting.on(move || {
            format!("{} cell(s) running", seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst))
        });
        tokio::time::sleep(Duration::from_secs(61)).await;
        waiting.done();
        tokio::time::sleep(Duration::from_secs(90)).await;
        assert_eq!(
            *said.lock().unwrap(),
            [
                "replay batch: waiting 30 s for 0 cell(s) running",
                "replay batch: waiting 60 s for 1 cell(s) running",
            ]
        );
    }
}
