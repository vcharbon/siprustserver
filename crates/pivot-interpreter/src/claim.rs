//! **Claiming** an inbound initial INVITE (`PCAP2TEST_PIVOT_V3.md` §5): which
//! of the legs sharing one socket the arriving dialog belongs to.
//!
//! `claim` carries `by` and nothing else — which attempt an actor plays is
//! already stated by `calls[].attempts[].leg`, so the claim states only what
//! TELLS the INVITEs apart. Two discriminators, and each is exact:
//!
//! - `ruri-pos` — the R-URI user equals a number the lane bound for the leg's
//!   own callee identity. Equality against a BOUND FORM, never a prefix or a
//!   suffix: a widened match here is a call delivered to the wrong actor.
//! - `arrival-order` — nothing distinguishes them, so the Nth arrival is the
//!   Nth candidate in declaration order. A claimed leg leaves the candidate
//!   list, so "the first still-open arrival-order candidate" IS the Nth, and a
//!   discriminated claim beside it never consumes a slot it did not take.
//!
//! A leg claims once. An INVITE no candidate claims is an UNCLAIMED arrival, and
//! the run says so rather than delivering it to whoever is next.

use std::collections::BTreeSet;

use pivot_schema::placement::ClaimBy;

use crate::gate::Inbound;

/// One leg that can take an inbound initial INVITE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub leg: String,
    pub actor: String,
    pub endpoint: String,
    pub by: ClaimBy,
    /// Every number the lane bound for this leg's callee identity, over every
    /// dial form the identity declares.
    pub numbers: BTreeSet<String>,
    /// Declaration order, which is what `arrival-order` counts in.
    pub order: usize,
}

/// Why an inbound INVITE was not claimed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimError {
    /// Every candidate on the endpoint has already claimed its dialog.
    AllClaimed { endpoint: String, arrived: String },
    /// No candidate's discriminator matches.
    NoMatch { endpoint: String, arrived: String, tried: Vec<String> },
}

impl std::fmt::Display for ClaimError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClaimError::AllClaimed { endpoint, arrived } => write!(
                f,
                "endpoint {endpoint}: every claiming leg already has its dialog; {arrived} arrived"
            ),
            ClaimError::NoMatch { endpoint, arrived, tried } => write!(
                f,
                "endpoint {endpoint}: {arrived} matches no claim ({})",
                tried.join(", ")
            ),
        }
    }
}

/// The claim table for one run: the candidates, and which have claimed.
#[derive(Debug, Clone, Default)]
pub struct ClaimIndex {
    candidates: Vec<Candidate>,
    claimed: BTreeSet<String>,
}

impl ClaimIndex {
    pub fn new(candidates: Vec<Candidate>) -> Self {
        ClaimIndex { candidates, claimed: BTreeSet::new() }
    }

    /// Whether `leg` has already taken its dialog.
    #[cfg(test)]
    pub fn has_claimed(&self, leg: &str) -> bool {
        self.claimed.contains(leg)
    }

    /// The leg an inbound initial INVITE on `endpoint` belongs to.
    ///
    /// Candidates are considered in declaration order. A `ruri-pos` candidate
    /// matches on its own discriminator; an `arrival-order` candidate matches
    /// when its ordinal among the endpoint's unclaimed arrival-order
    /// candidates equals this arrival's ordinal.
    pub fn claim(&mut self, endpoint: &str, ruri_user: &str, inbound: &Inbound) -> Result<String, ClaimError> {
        let open: Vec<&Candidate> = self
            .candidates
            .iter()
            .filter(|c| c.endpoint == endpoint && !self.claimed.contains(&c.leg))
            .collect();
        if open.is_empty() {
            return Err(ClaimError::AllClaimed {
                endpoint: endpoint.to_string(),
                arrived: inbound.describe(),
            });
        }
        let mut tried = Vec::new();
        let mut chosen: Option<String> = None;
        for candidate in open {
            match candidate.by {
                ClaimBy::RuriPos => {
                    tried.push(format!(
                        "{} by ruri-pos on {:?}",
                        candidate.leg,
                        candidate.numbers.iter().cloned().collect::<Vec<_>>()
                    ));
                    if candidate.numbers.iter().any(|n| n == ruri_user) {
                        chosen = Some(candidate.leg.clone());
                        break;
                    }
                }
                ClaimBy::ArrivalOrder => {
                    tried.push(format!("{} by arrival-order", candidate.leg));
                    chosen = Some(candidate.leg.clone());
                    break;
                }
            }
        }
        match chosen {
            Some(leg) => {
                self.claimed.insert(leg.clone());
                Ok(leg)
            }
            None => Err(ClaimError::NoMatch {
                endpoint: endpoint.to_string(),
                arrived: inbound.describe(),
                tried,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(leg: &str, by: ClaimBy, numbers: &[&str], order: usize) -> Candidate {
        Candidate {
            leg: leg.into(),
            actor: format!("uas{order}"),
            endpoint: "ep0".into(),
            by,
            numbers: numbers.iter().map(|n| n.to_string()).collect(),
            order,
        }
    }

    fn invite() -> Inbound {
        Inbound {
            method: Some("INVITE".into()),
            status: None,
            reason: None,
            cseq_method: "INVITE".into(),
            cseq: 1,
            call_id: "c".into(),
            rseq: None,
            headers: Default::default(),
            body: vec![],
            content_type: None,
            ruri_user: None,
            from_tag: Some("a1".into()),
            to_tag: Some("t7".into()),
            from_user: Some("0009001".into()),
            to_user: Some("bob".into()),
        }
    }

    #[test]
    fn ruri_pos_matches_a_bound_number_exactly() {
        let mut index = ClaimIndex::new(vec![
            candidate("B", ClaimBy::RuriPos, &["+33000900004", "0900004"], 0),
            candidate("C", ClaimBy::RuriPos, &["+33000900005"], 1),
        ]);
        assert_eq!(index.claim("ep0", "+33000900005", &invite()).unwrap(), "C");
        assert_eq!(index.claim("ep0", "0900004", &invite()).unwrap(), "B");
        // A leg claims once.
        assert!(index.has_claimed("B") && index.has_claimed("C"));
        assert!(matches!(
            index.claim("ep0", "0900004", &invite()),
            Err(ClaimError::AllClaimed { .. })
        ));
    }

    #[test]
    fn a_number_that_merely_contains_the_bound_one_does_not_claim() {
        let mut index = ClaimIndex::new(vec![candidate("B", ClaimBy::RuriPos, &["900004"], 0)]);
        let err = index.claim("ep0", "+331999999900004", &invite()).unwrap_err();
        assert!(matches!(err, ClaimError::NoMatch { .. }), "{err}");
        assert!(!index.has_claimed("B"), "a refused claim leaves the leg open");
    }

    #[test]
    fn arrival_order_gives_the_nth_arrival_to_the_nth_candidate() {
        let mut index = ClaimIndex::new(vec![
            candidate("B", ClaimBy::ArrivalOrder, &[], 0),
            candidate("C", ClaimBy::ArrivalOrder, &[], 1),
        ]);
        assert_eq!(index.claim("ep0", "x", &invite()).unwrap(), "B");
        assert_eq!(index.claim("ep0", "y", &invite()).unwrap(), "C");
    }

    #[test]
    fn a_discriminated_claim_beside_an_arrival_order_one_consumes_no_slot() {
        // B is told apart by its number, so it never takes C's turn: the first
        // arrival-order candidate still open is the one an unrecognized INVITE
        // belongs to.
        let mut index = ClaimIndex::new(vec![
            candidate("B", ClaimBy::RuriPos, &["0900004"], 0),
            candidate("C", ClaimBy::ArrivalOrder, &[], 1),
            candidate("D", ClaimBy::ArrivalOrder, &[], 2),
        ]);
        assert_eq!(index.claim("ep0", "0900004", &invite()).unwrap(), "B");
        assert_eq!(index.claim("ep0", "zz", &invite()).unwrap(), "C");
        assert_eq!(index.claim("ep0", "yy", &invite()).unwrap(), "D");
    }
}
