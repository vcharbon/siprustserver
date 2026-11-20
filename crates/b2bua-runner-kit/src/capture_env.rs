//! `B2BUA_CAPTURE` applied at bind: the worker's endpoint captured to the
//! pcapng file the variable names ([`sip_net::CaptureEndpoint`]).

use std::path::Path;
use std::sync::Arc;

use sip_net::{CaptureEndpoint, UdpEndpoint};

/// `endpoint`, its datagrams appended to the file `capture` names where it
/// names one. `Err` is the boot refusal of a file that cannot be opened,
/// naming `B2BUA_CAPTURE`. `name` names the service in the log line.
pub(crate) fn captured(
    endpoint: Arc<dyn UdpEndpoint>,
    capture: Option<&Path>,
    name: &str,
) -> Result<Arc<dyn UdpEndpoint>, String> {
    let Some(file) = capture else { return Ok(endpoint) };
    let captured = CaptureEndpoint::open(endpoint, file)
        .map_err(|e| format!("B2BUA_CAPTURE={}: {e}", file.display()))?;
    tracing::info!(service = name, capture = %file.display(), "capturing its SIP datagrams to a pcapng file");
    Ok(Arc::new(captured))
}
