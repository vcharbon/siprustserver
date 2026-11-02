//! [`RunnerBase::spawn`]: the core started on a configuration checked whole.

use std::sync::Arc;

use b2bua::rules::ServiceDef;
use b2bua::{B2buaCore, B2buaDeps};

use crate::{ConfigCheck, RunnerBase};

impl RunnerBase {
    /// Check the final composed configuration once — `check` holds the
    /// runner's own relations, the generic ones are added here
    /// ([`ConfigCheck`]) — then spawn the core over one boxed clone of the
    /// shared endpoint, composing `services` (ADR-0016) above the defaults.
    /// `Err` is the boot refusal naming every violated relation; nothing is
    /// served then.
    pub fn spawn(
        &self,
        deps: B2buaDeps,
        services: Vec<ServiceDef>,
        mut check: ConfigCheck,
    ) -> Result<Arc<B2buaCore>, String> {
        check.generic(&self.env, &deps);
        check.into_result()?;
        Ok(Arc::new(B2buaCore::spawn_with_services(
            Box::new(self.endpoint.clone()),
            deps,
            services,
        )))
    }
}
