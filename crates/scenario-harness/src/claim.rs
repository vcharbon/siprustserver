//! Scenario-defined claims — data-driven assignment of inbound legs on a
//! shared endpoint.
//!
//! Where a [`LegPicker`](crate::legpick::LegPicker) is a compiled-in callback,
//! a [`ClaimRule`] is pure DATA: a scenario built from a capture (the pcap2test
//! pivot's `actors[].claim`) declares, per call instance, which pending
//! receiver owns each inbound initial INVITE arriving in the call's
//! correlation-token scope. A claim fires exactly once (consumed); resolution
//! is [`resolve_claim`], shared by every consumer so the precedence is defined
//! in one place.

use crate::legpick::LegInfo;

/// One pending receiver's claim over an inbound initial INVITE, scoped to the
/// call's correlation token (never global). Rules cover the leg shapes a
/// multi-actor scenario expects back on a shared socket: a role number
/// (reroute alternate, MRF leg), a transfer-target INVITE (Replaces), or —
/// last resort, when the capture shows the same number on several legs —
/// plain arrival order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimRule {
    /// The R-URI user-part starts with this prefix (the role number as dialed
    /// on the wire; prefix + longest-match, the same semantics as
    /// [`labelled_prefix_leg_picker`](crate::legpick::labelled_prefix_leg_picker)).
    RuriUser(String),
    /// The INVITE carries a `Replaces` header (attended-transfer target leg).
    HasReplaces,
    /// Fires for the k-th (0-based) leg claimed BY ARRIVAL ORDER in the token
    /// scope — the last-resort rule for legs nothing else distinguishes (the
    /// capture shows the same number on primary and alternate). Only fired
    /// ArrivalOrder claims advance the ordinal: legs claimed by a more
    /// specific rule, retransmissions, and unclaimed strays never shift it,
    /// so order claims stay stable however the SUT interleaves the other legs.
    ArrivalOrder(usize),
}

/// Resolve which pending claim owns `leg` — the index into `claims` — or
/// `None` when no claim matches (the caller counts it, distinctly from a true
/// orphan). `claims[i] = None` marks an already-consumed (or claim-less) slot;
/// `ordinal` is the number of [`ClaimRule::ArrivalOrder`] claims already fired
/// in this token scope (the caller advances it only when the returned index
/// held an ArrivalOrder claim).
///
/// Precedence is most-specific-first, and absence falls through: a
/// Replaces-carrying INVITE goes to a pending [`ClaimRule::HasReplaces`] claim
/// first, else it competes by R-URI like any other leg; then the LONGEST
/// matching [`ClaimRule::RuriUser`] prefix wins (a userless R-URI matches no
/// prefix); finally a [`ClaimRule::ArrivalOrder`] equal to `ordinal`.
pub fn resolve_claim(
    claims: &[Option<&ClaimRule>],
    leg: &LegInfo,
    ordinal: usize,
) -> Option<usize> {
    if leg.header("Replaces").is_some() {
        if let Some(i) = claims
            .iter()
            .position(|c| matches!(c, Some(ClaimRule::HasReplaces)))
        {
            return Some(i);
        }
    }
    if let Some(user) = leg.ruri_user() {
        let mut best: Option<(usize, usize)> = None; // (index, prefix len)
        for (i, c) in claims.iter().enumerate() {
            if let Some(ClaimRule::RuriUser(prefix)) = c {
                if user.starts_with(prefix.as_str())
                    && best.is_none_or(|(_, len)| prefix.len() > len)
                {
                    best = Some((i, prefix.len()));
                }
            }
        }
        if let Some((i, _)) = best {
            return Some(i);
        }
    }
    claims
        .iter()
        .position(|c| matches!(c, Some(ClaimRule::ArrivalOrder(k)) if *k == ordinal))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invite(ruri: &str, extra: &str) -> Vec<u8> {
        format!(
            "INVITE {ruri} SIP/2.0\r\nCall-ID: c1@h\r\n{extra}To: <{ruri}>\r\n\
             From: <sip:a@h>;tag=1\r\nCSeq: 1 INVITE\r\n\r\n"
        )
        .into_bytes()
    }

    fn resolve(claims: &[Option<&ClaimRule>], raw: &[u8], ordinal: usize) -> Option<usize> {
        resolve_claim(claims, &LegInfo::new(raw), ordinal)
    }

    /// The longest matching R-URI prefix wins across pending claims (number
    /// forms nest — a full transfer number vs its sibling prefix), and a
    /// consumed slot (`None`) no longer competes.
    #[test]
    fn ruri_user_longest_prefix_wins_and_consumed_slots_are_skipped() {
        let short = ClaimRule::RuriUser("0650033033".into());
        let long = ClaimRule::RuriUser("0650033033231".into());
        let raw = invite("sip:0650033033231089@10.0.0.1:5070", "");

        assert_eq!(resolve(&[Some(&short), Some(&long)], &raw, 0), Some(1));
        // The long claim consumed → the sibling prefix now owns the leg.
        assert_eq!(resolve(&[Some(&short), None], &raw, 0), Some(0));
        // Nothing pending matches → unclaimed, not first-wins.
        assert_eq!(resolve(&[None, None], &raw, 0), None);
        let other = invite("sip:999@10.0.0.1:5070", "");
        assert_eq!(resolve(&[Some(&short), Some(&long)], &other, 0), None);
    }

    /// A Replaces-carrying INVITE goes to the pending HasReplaces claim ahead
    /// of a matching R-URI claim; with no HasReplaces pending it falls through
    /// and competes by R-URI like any other leg.
    #[test]
    fn has_replaces_takes_precedence_then_falls_through() {
        let by_number = ClaimRule::RuriUser("0650".into());
        let xfer = ClaimRule::HasReplaces;
        let raw = invite(
            "sip:065012@10.0.0.1:5070",
            "Replaces: abc@h;to-tag=1;from-tag=2\r\n",
        );

        assert_eq!(resolve(&[Some(&by_number), Some(&xfer)], &raw, 0), Some(1));
        assert_eq!(resolve(&[Some(&by_number), None], &raw, 0), Some(0));
        // A plain INVITE never matches HasReplaces.
        let plain = invite("sip:99@10.0.0.1:5070", "");
        assert_eq!(resolve(&[None, Some(&xfer)], &plain, 0), None);
    }

    /// ArrivalOrder(k) fires for the k-th claimed leg — the same-number
    /// primary/alternate reroute — and a userless R-URI (the SUT's bare route
    /// target) is claimable ONLY by arrival order.
    #[test]
    fn arrival_order_claims_kth_leg_and_userless_legs() {
        let first = ClaimRule::ArrivalOrder(0);
        let second = ClaimRule::ArrivalOrder(1);
        let raw = invite("sip:0590777@10.0.0.1:5070", "");

        assert_eq!(resolve(&[Some(&first), Some(&second)], &raw, 0), Some(0));
        assert_eq!(resolve(&[None, Some(&second)], &raw, 1), Some(1));
        // Ordinal mismatch (a leg arriving out of declared order) → unclaimed.
        assert_eq!(resolve(&[None, Some(&second)], &raw, 0), None);

        let userless = invite("sip:192.168.60.20:6001", "");
        let by_number = ClaimRule::RuriUser("0590".into());
        assert_eq!(resolve(&[Some(&by_number), Some(&first)], &userless, 0), Some(1));
    }

    /// A specific rule beats arrival order regardless of declaration position
    /// — a specifically-claimed leg never consumes an ordinal.
    #[test]
    fn specific_rules_beat_arrival_order() {
        let order0 = ClaimRule::ArrivalOrder(0);
        let by_number = ClaimRule::RuriUser("0491".into());
        let mrf = invite("sip:049112@10.0.0.1:5070", "");
        assert_eq!(resolve(&[Some(&order0), Some(&by_number)], &mrf, 0), Some(1));
    }
}
