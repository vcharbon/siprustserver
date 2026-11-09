//! [`RunnerBase::spawn`]: the core started on a configuration checked whole,
//! under the process's poll watchdog (ADR-0057 §6).

use std::sync::Arc;

use b2bua::rules::ServiceDef;
use b2bua::{B2buaCore, B2buaDeps};

use crate::{ConfigCheck, RunnerBase};

impl RunnerBase {
    /// Check the final composed configuration once — `check` holds the
    /// runner's own relations, the generic ones are added here
    /// ([`ConfigCheck`]) — then spawn the core over one boxed clone of the
    /// shared endpoint, composing `services` (ADR-0016) above the defaults,
    /// and start the poll watchdog over it. `Err` is the boot refusal naming
    /// every violated relation, or a thread's CPU time that does not read;
    /// nothing is served then.
    pub fn spawn(
        &self,
        deps: B2buaDeps,
        services: Vec<ServiceDef>,
        mut check: ConfigCheck,
    ) -> Result<Arc<B2buaCore>, String> {
        check.generic(&self.env, &deps);
        check.into_result()?;
        let watchdog = poll_watchdog::Watchdog::process();
        watchdog
            .start(self.env.stuck_poll_cpu)
            .map_err(|e| format!("the poll watchdog cannot read a thread's CPU time: {e}"))?;
        let endpoint = Box::new(self.endpoint.clone());
        let core = Arc::new(B2buaCore::spawn_with_services(endpoint, deps, services));
        crate::shutdown::watchdog_serves(watchdog, &core, self.env.drain);
        Ok(core)
    }
}
