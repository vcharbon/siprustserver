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
use crate::types::ReadRoutes;

/// The Route and Record-Route entries of the message, or why one of them does
/// not read.
pub(super) fn read_routes(idx: &HeaderIndex) -> Result<ReadRoutes, SipParseError> {
    let mut routes = Vec::with_capacity(idx.route.len());
    for line in &idx.route {
        RouteEntry::parse_line_into(line, &mut routes)
            .map_err(|e| SipParseError::new(format!("Strict Route: {}", e.reason)))?;
    }
    let mut recorded = Vec::with_capacity(idx.record_route.len());
    for line in &idx.record_route {
        let read = recorded.len();
        RecordRouteEntry::parse_line_into(line, &mut recorded)
            .map_err(|e| SipParseError::new(format!("Strict Record-Route: {}", e.reason)))?;
        for entry in recorded.get(read..).unwrap_or_default() {
            request_uri_refusal(entry.uri())?;
        }
    }
    Ok(ReadRoutes { sets: Some((routes, recorded)) })
}
