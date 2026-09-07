//! RFC-violation rules over an observed SIP wire — the ONE home for this
//! knowledge (issue 29). A rule body here is written once against the wire
//! model in [`wire`] and consumed by two adapters: the capture-side census
//! (`sip_pcap::rfc`, reading pcap flows documents) and the live recorded-trace
//! audit (`sip_net::rfc_audit`, reading the harness event channel). What
//! differs between those consumers is OBSERVATION POLICY — how the stream was
//! seen, whether the recording is complete, what to do with an undecidable or
//! relay-attributed finding — and all of it is data on the model
//! ([`wire::Observation`], [`verdict::Decision`], `Finding::relayed`), never a
//! property of which module a rule happens to live in.
//!
//! **A rule decides an OCCASION three ways** ([`verdict::Decision`]):
//! violated with evidence, compliant, or undecidable — and the population
//! contract `hits ⊆ decided ⊆ occasions` makes the cost of conservatism
//! visible on every report. Consumers choose their policy over `Undecidable`
//! (the census under-reports; the live gate may still fail hard) — the rule
//! itself only ever states what the wire proves.

pub mod rules;
pub mod verdict;
pub mod wire;

pub use verdict::{Decision, Evidence, Finding, Population, RuleId};
pub use wire::{Endpoint, Kind, Msg, Observation, WireView};

/// The rules an adapter runs, in report order.
pub fn all_rules() -> Vec<Box<dyn rules::Obligation>> {
    vec![
        Box::new(rules::cancel::No200AfterCancel),
        Box::new(rules::ack::NoAckToDialogCreating2xx),
        Box::new(rules::ack::Unacked2xxNotCleared),
        Box::new(rules::prack::UnackedReliableProvisional),
        Box::new(rules::prack::RackWithoutKnownInvite),
        Box::new(rules::prack::NoOverlappingReliableProvisionals),
        Box::new(rules::prack::NonContiguousRseq),
        Box::new(rules::prack::NoPrackOfOutOfOrderRseq),
        Box::new(rules::final_response::SingleFinalPerServerTxn),
        Box::new(rules::cancel::CancelRouteEchoesInvite),
        Box::new(rules::cancel::CancelAfter1xx),
        Box::new(rules::cancel::NoCancelAfterFinal),
        Box::new(rules::cseq::CseqInDialogOrder),
        Box::new(rules::cseq::ResponseCseqMatchesTransaction),
        Box::new(rules::cseq::AckCseqMatchesInvite),
        Box::new(rules::dialog::MidDialogUri),
        Box::new(rules::dialog::MidDialogRoute),
        Box::new(rules::dialog::MidDialogWireDestination),
        Box::new(rules::dialog::RecordRoutePlacement),
        Box::new(rules::via::RportEcho),
        Box::new(rules::capability::AllowSupportedOnInvite),
        Box::new(rules::proxy::Proxy100TryingNotForwarded),
        Box::new(rules::dialog::UnknownDialog481),
        Box::new(rules::capability::UnsupportedMethod405Allow),
        Box::new(rules::capability::UnsupportedExtension420),
        Box::new(rules::capability::Unsupported415Accepts),
        Box::new(rules::capability::UnsupportedExtension421),
        Box::new(rules::proxy::NoTarget404),
        Box::new(rules::capability::OptionsResponseEchoes),
        Box::new(rules::ack::AckRequireSubsetOfInvite),
        Box::new(rules::ack::AckPreservesInviteRoute),
        Box::new(rules::proxy::StrictRouteRewriteHandled),
        Box::new(rules::register::SerialRegister),
        Box::new(rules::register::RegisterNoRouteSet),
        Box::new(rules::reinvite::ConcurrentReInvite500Or491),
        Box::new(rules::dialog::NoByeOutsideOrEarlyDialog),
        Box::new(rules::reinvite::NoReInviteWhileInviteInProgress),
        Box::new(rules::proxy::Proxy100WithinGrace),
        Box::new(rules::ack::UnackedInviteNon2xxFinal),
        Box::new(rules::reinvite::FailedReinviteTearsDownDialog),
        Box::new(rules::final_response::No1xxAfterFinal),
        Box::new(rules::prack::RequireReliable1xxOnRequire),
        Box::new(rules::prack::ReliableNeedsClientOptIn),
        Box::new(rules::prack::NoReliable1xxOnInDialog),
        Box::new(rules::prack::UnmatchedPrackProxied),
        Box::new(rules::prack::Prack2xxOr481),
        Box::new(rules::prack::Delay2xxOnUnackedReliable1xxWithSdp),
        Box::new(rules::prack::PrackAcceptedAfterFinal),
        Box::new(rules::prack::NoNewReliable1xxAfterFinal),
        Box::new(rules::prack::NoPrackOf100Trying),
        Box::new(rules::prack::PrackAnswers1xxOffer),
        Box::new(rules::offer_answer::AckBodyAfterCompleteOfferAnswer),
        Box::new(rules::offer_answer::Final2xxAnswersTheOffer),
        Box::new(rules::offer_answer::SecondAnswerRepeatsTheFirst),
        Box::new(rules::offer_answer::AnswerStreamMatchesOffer),
        Box::new(rules::offer_answer::SdpOriginContinuity),
        Box::new(rules::offer_answer::NoNewOfferWhileOfferPending),
        Box::new(rules::offer_answer::AnswerMLineCountMatchesOffer),
        Box::new(rules::offer_answer::AnswerTLineEqualsOffer),
        Box::new(rules::offer_answer::AnswerMediaTypeMatchesOffer),
        Box::new(rules::offer_answer::DirectionPairValid),
        Box::new(rules::offer_answer::RejectedStreamMinimalAnswer),
        Box::new(rules::offer_answer::ReOfferMLineCountMonotonic),
        Box::new(rules::offer_answer::ZeroPortPropagation),
        Box::new(rules::offer_answer::PayloadTypeMappingStable),
        Box::new(rules::wellformed::BranchPrefix),
        Box::new(rules::wellformed::MaxForwards),
        Box::new(rules::wellformed::ContentLength),
        Box::new(rules::wellformed::ContentType),
        Box::new(rules::wellformed::ContactPresence),
        Box::new(rules::wellformed::NoContactOnBye),
        Box::new(rules::wellformed::ToTagPresence),
        Box::new(rules::wellformed::NoRecordRouteFromUa),
        Box::new(rules::correlation::ResponseEchoesRequestVia),
        Box::new(rules::correlation::ResponseCorrelation),
        Box::new(rules::correlation::MidDialogTags),
        Box::new(rules::correlation::PeerUriStable),
        Box::new(rules::correlation::DialogCallIdStable),
        Box::new(rules::correlation::CancelRequestUri),
        Box::new(rules::correlation::CancelViaBranch),
        Box::new(rules::correlation::TagConsistency),
        Box::new(rules::wellformed::No100relRequireOnNonInvite),
        Box::new(rules::wellformed::Reliable1xxHeaders),
        Box::new(rules::wellformed::CancelCseqMethod),
        Box::new(rules::correlation::NoToTagOnInitialRequest),
        Box::new(rules::correlation::InDialogToTag),
        Box::new(rules::capability::NoRequireOnCancelOrAck),
        Box::new(rules::proxy::StrictRouteShuffleOnSend),
        Box::new(rules::offer_answer::SdpBodyParseable),
        Box::new(rules::offer_answer::C0PortNonZero),
        Box::new(rules::retransmit::RungByteIdentical),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two registries are one fact stated twice: a rule with a body is in
    /// [`RuleId::ALL`], and every member of `ALL` has a body here. A rung that
    /// adds a variant and forgets `all_rules()` (or the reverse) fails here
    /// rather than in a consumer that silently never runs it.
    #[test]
    fn every_rule_with_a_body_is_registered_once() {
        let mut registered: Vec<RuleId> = all_rules().iter().map(|r| r.id()).collect();
        registered.sort();
        let before = registered.len();
        registered.dedup();
        assert_eq!(before, registered.len(), "a rule is registered twice");
        let mut declared = RuleId::ALL.to_vec();
        declared.sort();
        assert_eq!(registered, declared);
    }

    /// Every token round-trips, and no two rules share one.
    #[test]
    fn tokens_are_unique_and_parse_back() {
        let mut tokens: Vec<&str> = RuleId::ALL.iter().map(|r| r.token()).collect();
        tokens.sort_unstable();
        let before = tokens.len();
        tokens.dedup();
        assert_eq!(before, tokens.len(), "two rules share a token");
        for rule in RuleId::ALL {
            assert_eq!(rule.token().parse::<RuleId>().unwrap(), *rule);
        }
    }
}
