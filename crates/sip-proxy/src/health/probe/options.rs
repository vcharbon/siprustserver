//! The OPTIONS one probe sends.

use sip_message::generators::{
    generate_out_of_dialog_request, GenerateOutOfDialogRequestOpts, OutOfDialogMethod,
};
use sip_message::header::{self, ParamValue, Uri, Via};
use sip_message::{SipRequest, SipStr};

use super::{probe_call_ref, HealthProbe};
use crate::registry::WorkerEntry;

impl HealthProbe {
    /// The OPTIONS probing `w`; `None` where its address does not make one.
    pub(super) fn build_options(
        &self,
        w: &WorkerEntry,
        branch: &str,
        now_ms: u64,
    ) -> Option<SipRequest> {
        generate_out_of_dialog_request(
            OutOfDialogMethod::Options,
            &GenerateOutOfDialogRequestOpts {
                request_uri: Some(
                    Uri::sip(SipStr::owned(&w.address.host)).with_port(w.address.port),
                ),
                // Diagnostic only — correlation is the transaction branch.
                call_id: format!("probe-{}-{}@{}", w.id, now_ms, self.probe_host),
                from: Some(
                    header::From::from_uri(Uri::sip_user(
                        SipStr::from_static("probe"),
                        SipStr::owned(&self.probe_host),
                    ))
                    .with_tag(SipStr::from_static("probe")),
                ),
                to: Some(header::To::from_uri(
                    Uri::sip_user(SipStr::from_static("probe"), SipStr::owned(&w.address.host))
                        .with_port(w.address.port),
                )),
                cseq: 1,
                via: Some(
                    Via::udp(SipStr::owned(&self.probe_host), self.probe_port)
                        .with_branch(SipStr::owned(branch))
                        .requesting_rport()
                        .with_param(
                            "cr",
                            ParamValue::Token(SipStr::owned(&probe_call_ref(branch))),
                        ),
                ),
                contact: Some(header::Contact::from_uri(
                    Uri::sip_user(SipStr::from_static("probe"), SipStr::owned(&self.probe_host))
                        .with_port(self.probe_port),
                )),
                max_forwards: Some(70),
                ..Default::default()
            },
        )
        .inspect_err(|e| {
            tracing::warn!(worker = %w.id, error = %e, "health probe not sent: its address makes no OPTIONS");
        })
        .ok()
    }
}
