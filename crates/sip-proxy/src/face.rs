//! Which face a message leaves on (ADR-0048, ADR-0056).
//!
//! A dual-face proxy bridges two planes, the workers' and the callers', with
//! one UDP socket and one advertise per face. A message crosses: what arrives
//! on one face leaves on the other. Two exceptions come from the registry, so a
//! call looping back through the platform stays on the workers' plane, and an
//! own final leaves from the face that received its request (RFC 3581 §4).
//!
//! The inputs are the ingress face and what the message and the registry say,
//! so any proxy instance picks the same egress with nothing stored: the choice
//! survives a flow move and a takeover, and a request's copies, its CANCEL and
//! its ACK state one sent-by (RFC 3261 §17.2.3).

use crate::observability::metrics::Face;

/// The face-picking policy. Only a dual-face proxy crosses; on a single-face
/// proxy every message stays on its one face.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EgressPolicy {
    SingleFace,
    Crossing,
}

impl EgressPolicy {
    /// The face a forwarded request leaves on. `target_is_worker` is asked only
    /// of a request arriving internal that is neither `worker_outbound` nor a
    /// `loose` hop, the one row that reads the registry: such a request bound
    /// for a worker re-enters the platform.
    pub(crate) fn request(
        self,
        ingress: Face,
        worker_outbound: bool,
        loose: bool,
        target_is_worker: impl FnOnce() -> bool,
    ) -> Face {
        match (self, ingress) {
            (Self::SingleFace, _) | (Self::Crossing, Face::External) => Face::Internal,
            (Self::Crossing, Face::Internal) if worker_outbound || loose => Face::External,
            (Self::Crossing, Face::Internal) if target_is_worker() => Face::Internal,
            (Self::Crossing, Face::Internal) => Face::External,
        }
    }

    /// The face a relayed response leaves on. `to_worker`: the owner rule names
    /// a worker, or the response's next Via's sent-by is a registry worker.
    /// That sent-by and not the `received` beside it: a translated worker's
    /// `received` is a node address.
    pub(crate) fn response(self, ingress: Face, to_worker: bool) -> Face {
        match (self, ingress) {
            (Self::SingleFace, _) | (Self::Crossing, Face::External) => Face::Internal,
            (Self::Crossing, Face::Internal) if to_worker => Face::Internal,
            (Self::Crossing, Face::Internal) => Face::External,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::EgressPolicy::{Crossing, SingleFace};
    use crate::observability::metrics::Face::{External, Internal};

    #[test]
    fn a_single_face_proxy_stays_on_its_face() {
        for (outbound, loose, worker) in [(false, false, false), (true, true, true)] {
            assert_eq!(SingleFace.request(Internal, outbound, loose, || worker), Internal);
            assert_eq!(SingleFace.response(Internal, worker), Internal);
        }
    }

    #[test]
    fn a_request_crosses_but_a_re_entry() {
        let table = [
            (External, false, false, false, Internal),
            (External, true, true, true, Internal),
            (Internal, true, false, true, External),
            (Internal, false, true, true, External),
            (Internal, false, false, true, Internal),
            (Internal, false, false, false, External),
        ];
        for (ingress, outbound, loose, worker, egress) in table {
            assert_eq!(Crossing.request(ingress, outbound, loose, || worker), egress);
        }
    }

    #[test]
    fn the_registry_is_read_only_for_the_row_that_needs_it() {
        let unread = || -> bool { panic!("the registry is not read on this row") };
        assert_eq!(Crossing.request(External, false, false, unread), Internal);
        assert_eq!(Crossing.request(Internal, true, false, unread), External);
        assert_eq!(Crossing.request(Internal, false, true, unread), External);
        assert_eq!(SingleFace.request(Internal, false, false, unread), Internal);
    }

    #[test]
    fn a_response_crosses_but_one_for_a_worker() {
        assert_eq!(Crossing.response(External, false), Internal);
        assert_eq!(Crossing.response(External, true), Internal);
        assert_eq!(Crossing.response(Internal, true), Internal);
        assert_eq!(Crossing.response(Internal, false), External);
    }
}
