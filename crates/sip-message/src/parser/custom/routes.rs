//! The route sets a message carries, read on wire bytes at parse time: every
//! Route and Record-Route entry is a name-addr whose URI the one URI reader
//! takes (RFC 3261 §20.30, §20.34), and a recorded route is a URI a request
//! can be addressed to (§12.2.1.1). The typed reads on access
//! ([`route_set`](crate::SipRequest::route_set),
//! [`record_route_set`](crate::SipRequest::record_route_set)) use the same
//! readers.

use super::header_index::HeaderIndex;
use super::request_uri::request_uri_refusal;
use crate::error::SipParseError;
use crate::header::{HeaderValue, RecordRouteEntry, RouteEntry};

/// Why a Route or Record-Route line of the message does not read, if one
/// does not.
pub(super) fn route_refusal(idx: &HeaderIndex) -> Result<(), SipParseError> {
    for line in &idx.route {
        RouteEntry::parse_line(line)
            .map_err(|e| SipParseError::new(format!("Strict Route: {}", e.reason)))?;
    }
    for line in &idx.record_route {
        let recorded = RecordRouteEntry::parse_line(line)
            .map_err(|e| SipParseError::new(format!("Strict Record-Route: {}", e.reason)))?;
        for entry in &recorded {
            request_uri_refusal(entry.uri())?;
        }
    }
    Ok(())
}
