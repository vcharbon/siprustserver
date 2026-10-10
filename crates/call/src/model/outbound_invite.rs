//! The dialog-creating INVITE a leg sent ([`OutboundInvite`]): what each
//! dialog a response to it creates starts from (RFC 3261 §12.1.2).

use serde::{Deserialize, Serialize};

/// The INVITE this stack sent to open a leg. Every dialog its responses
/// create carries `our_tag`, its From-tag, and starts its own sequence at
/// `cseq`, the INVITE's (§12.1.2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundInvite {
    pub our_tag: String,
    pub cseq: i64,
}
