//! [`ScriptedHttpService`]: serves added [`HttpScript`]s.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::error::HttpScriptError;
use super::program::{HttpBindings, HttpScript};
use super::verdict::{HttpFinding, HttpScriptHandle, HttpVerdict};
use crate::{HttpAnswer, HttpRequest, HttpResponse, HttpService};

/// The state every clone of a service and every handle share.
pub(super) struct Shared {
    nonce: u64,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    next_instance: u64,
}

impl Shared {
    pub(super) fn verdict(&self, instance: u64) -> HttpVerdict {
        let _ = (&self.state, self.nonce);
        todo!("verdict of instance {instance}")
    }
}

/// An [`HttpService`] serving the scripts added to it.
#[derive(Clone)]
pub struct ScriptedHttpService {
    shared: Arc<Shared>,
}

impl Default for ScriptedHttpService {
    fn default() -> Self {
        Self::new()
    }
}

impl ScriptedHttpService {
    /// A service with a random token nonce.
    pub fn new() -> Self {
        Self::with_nonce(0)
    }

    /// A service whose tokens carry `nonce`.
    pub fn with_nonce(nonce: u64) -> Self {
        Self { shared: Arc::new(Shared { nonce, state: Mutex::new(State::default()) }) }
    }

    /// Add one instance of `script`, its `${bind:…}` resolved from `bindings`.
    pub fn add(
        &self,
        script: HttpScript,
        bindings: HttpBindings,
    ) -> Result<HttpScriptHandle, HttpScriptError> {
        let _ = (script, bindings);
        let mut state = self.shared.state.lock().unwrap();
        let instance = state.next_instance;
        state.next_instance += 1;
        Ok(HttpScriptHandle { shared: self.shared.clone(), instance, attributable: false })
    }

    /// Every finding so far.
    pub fn findings(&self) -> Vec<HttpFinding> {
        todo!()
    }
}

#[async_trait]
impl HttpService for ScriptedHttpService {
    async fn handle(&self, req: HttpRequest) -> HttpResponse {
        match self.answer(req).await {
            HttpAnswer::Response(resp) => resp,
            HttpAnswer::Abort => panic!("Reset needs a transport calling HttpService::answer"),
        }
    }

    async fn answer(&self, _req: HttpRequest) -> HttpAnswer {
        todo!()
    }
}
