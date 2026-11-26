//! [`RelayedOptionsAdvertisement`]: what a 2xx to an OPTIONS the stack relays
//! in a dialog advertises.

/// What the 2xx to an in-dialog OPTIONS this stack RELAYED states as `Allow` /
/// `Supported` / `Accept` (RFC 3261 §11.2). A 2xx the stack answers itself
/// states the node's set whatever this says.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RelayedOptionsAdvertisement {
    /// The node's own set (`B2buaConfig::node_capabilities`) in place of the
    /// answerer's lines, and no `Accept-Encoding` / `Accept-Language`: the
    /// back-to-back UA is the UAS the OPTIONS asked.
    #[default]
    NodeSet,
    /// The answerer's advertisement as it came, every line passed through and
    /// none added: the far UAS answered the query, and a bare answer stays bare.
    AsAnswered,
}
