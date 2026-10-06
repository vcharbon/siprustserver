//! A decorator that records every request it forwards.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use b2bua::limiter::{
    AdmitOutcome, CallLimiter, LimiterEntry, LimiterHeld, LimiterReports, RefreshAnswer,
    RefreshCall, ReleaseAnswer,
};

/// `inner`, recording every request as it arrives, before it is forwarded.
pub fn spy(inner: Arc<dyn CallLimiter>) -> Arc<Spy> {
    Arc::new(Spy { inner, log: Mutex::default() })
}

/// One admit a [`Spy`] forwarded, as the caller sent it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpiedAdmit {
    pub key: String,
    pub change: u64,
    pub entries: Vec<LimiterEntry>,
    pub release_on_refusal: bool,
}

/// The limiter [`spy`] returns; its accessors read what it recorded so far.
pub struct Spy {
    inner: Arc<dyn CallLimiter>,
    log: Mutex<Log>,
}

#[derive(Default)]
struct Log {
    admits: Vec<SpiedAdmit>,
    releases: Vec<Vec<String>>,
    refreshes: usize,
}

impl Spy {
    /// Every admit, in arrival order.
    pub fn admits(&self) -> Vec<SpiedAdmit> {
        self.log.lock().unwrap().admits.clone()
    }

    /// The key of every admit, in arrival order.
    pub fn admitted_keys(&self) -> Vec<String> {
        self.log.lock().unwrap().admits.iter().map(|a| a.key.clone()).collect()
    }

    /// Every key of every release request, in arrival order.
    pub fn released_keys(&self) -> Vec<String> {
        self.log.lock().unwrap().releases.concat()
    }

    /// How many refresh requests arrived.
    pub fn refresh_requests(&self) -> usize {
        self.log.lock().unwrap().refreshes
    }

    /// Whether any request (admit, release or refresh) arrived.
    pub fn touched(&self) -> bool {
        let log = self.log.lock().unwrap();
        !log.admits.is_empty() || !log.releases.is_empty() || log.refreshes > 0
    }
}

#[async_trait]
impl CallLimiter for Spy {
    fn admit_budget(&self) -> Duration {
        self.inner.admit_budget()
    }

    async fn admit(
        &self,
        key: &str,
        change: u64,
        held: &LimiterHeld,
        entries: &[LimiterEntry],
        release_on_refusal: bool,
    ) -> AdmitOutcome {
        self.log.lock().unwrap().admits.push(SpiedAdmit {
            key: key.to_string(),
            change,
            entries: entries.to_vec(),
            release_on_refusal,
        });
        self.inner.admit(key, change, held, entries, release_on_refusal).await
    }

    async fn release(&self, keys: &[String]) -> ReleaseAnswer {
        self.log.lock().unwrap().releases.push(keys.to_vec());
        self.inner.release(keys).await
    }

    async fn refresh(&self, calls: &[RefreshCall]) -> RefreshAnswer {
        self.log.lock().unwrap().refreshes += 1;
        self.inner.refresh(calls).await
    }

    fn report_to(&self, reports: LimiterReports) {
        self.inner.report_to(reports);
    }
}
