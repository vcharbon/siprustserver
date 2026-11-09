//! Where each kind of evidence points: its own message in the view.

use super::Evidence;

impl Evidence {
    /// The evidence's own message index in its view, so a report reads in
    /// observation order whichever rule decided it.
    pub fn anchor(&self) -> usize {
        match self {
            Evidence::Cancelled { response_msg, .. } => *response_msg,
            Evidence::Unacked { provisional_msg, .. }
            | Evidence::Overlapping { provisional_msg, .. }
            | Evidence::RseqGap { provisional_msg, .. } => *provisional_msg,
            Evidence::UnknownRack { prack_msg, .. }
            | Evidence::OutOfOrderRack { prack_msg, .. } => *prack_msg,
            Evidence::NoAck { final_msg, .. } => *final_msg,
            Evidence::Uncleared { final_msg, .. } => *final_msg,
            Evidence::MultipleFinals { second_msg, .. } => *second_msg,
            Evidence::CancelRouteDiverged { cancel_msg, .. }
            | Evidence::EagerCancel { cancel_msg, .. }
            | Evidence::LateCancel { cancel_msg, .. } => *cancel_msg,
            Evidence::CseqReused { reuse_msg, .. } => *reuse_msg,
            Evidence::CseqNotContiguous { skip_msg, .. } => *skip_msg,
            Evidence::ResponseCseqUnmatched { mismatch_msg, .. } => *mismatch_msg,
            Evidence::AckCseqUnmatched { ack_msg, .. } => *ack_msg,
            Evidence::MidDialogUriChanged { uri_msg, .. } => *uri_msg,
            Evidence::MidDialogRouteDiverged { route_msg, .. } => *route_msg,
            Evidence::MidDialogWireTargetDiverged { wire_msg, .. } => *wire_msg,
            Evidence::RecordRouteMisplaced { record_route_msg, .. } => *record_route_msg,
            Evidence::RportNotEchoed { rport_msg, .. } => *rport_msg,
            Evidence::ResponseDestinationDiverged { response_dest_msg, .. } => *response_dest_msg,
            Evidence::CapabilitiesNotAdvertised { capability_msg, .. } => *capability_msg,
            Evidence::ExtraTryingForwarded { trying_msg, .. } => *trying_msg,
            Evidence::UnknownDialogRequest { unknown_dialog_msg, .. } => *unknown_dialog_msg,
            Evidence::RejectionNotIssued { rejection_msg, .. } => *rejection_msg,
            Evidence::ResponseHeadersMissing { response_headers_msg, .. } => *response_headers_msg,
            Evidence::NoTargetFinal { no_target_msg, .. } => *no_target_msg,
            Evidence::AckRequireNotSubset { ack_require_msg, .. } => *ack_require_msg,
            Evidence::AckRouteDiverged { ack_route_msg, .. } => *ack_route_msg,
            Evidence::StrictRouteNotRewritten { strict_route_msg, .. } => *strict_route_msg,
            Evidence::RegisterCarriesRoute { register_route_msg, .. } => *register_route_msg,
            Evidence::ConcurrentRegister { concurrent_register_msg, .. } => {
                *concurrent_register_msg
            }
            Evidence::ConcurrentReInvite { concurrent_invite_msg, .. } => *concurrent_invite_msg,
            Evidence::ByeOffDialog { bye_msg, .. } => *bye_msg,
            Evidence::OverlappingReInvite { overtaking_invite_msg, .. } => *overtaking_invite_msg,
            Evidence::TryingNotSentInGrace { trying_owed_msg, .. } => *trying_owed_msg,
            Evidence::UnackedReject { reject_msg, .. } => *reject_msg,
            Evidence::AbandonedReInvite { abandoned_invite_msg, .. } => *abandoned_invite_msg,
            Evidence::LateProvisional { late_provisional_msg, .. } => *late_provisional_msg,
            Evidence::Unreliable1xx { unreliable_1xx_msg, .. } => *unreliable_1xx_msg,
            Evidence::UnsolicitedReliable1xx { unsolicited_1xx_msg, .. } => *unsolicited_1xx_msg,
            Evidence::InDialogReliable1xx { in_dialog_1xx_msg, .. } => *in_dialog_1xx_msg,
            Evidence::PrackAbsorbed { absorbed_prack_msg, .. } => *absorbed_prack_msg,
            Evidence::PrackAnsweredWrongly { prack_answer_msg, .. } => *prack_answer_msg,
            Evidence::AnsweredOverUnackedOffer { early_2xx_msg, .. } => *early_2xx_msg,
            Evidence::Reliable1xxAfterFinal { stray_1xx_msg, .. } => *stray_1xx_msg,
            Evidence::PrackedTrying { trying_prack_msg, .. } => *trying_prack_msg,
            Evidence::PrackWithoutAnswer { bodiless_prack_msg, .. } => *bodiless_prack_msg,
            Evidence::AckBodyOnClosedRound { ack_body_msg, .. } => *ack_body_msg,
            Evidence::OfferLeftUnanswered { unanswered_final_msg, .. } => *unanswered_final_msg,
            Evidence::DelayedOfferUnanswered { unanswering_ack_msg, .. } => *unanswering_ack_msg,
            Evidence::SecondAnswerDiverged { second_answer_msg, .. } => *second_answer_msg,
            Evidence::AnswerStreamRetyped { answer_stream_msg, .. } => *answer_stream_msg,
            Evidence::SdpOriginDiverged { origin_msg, .. } => *origin_msg,
            Evidence::OfferWhilePending { new_offer_msg, .. } => *new_offer_msg,
            Evidence::AnswerMLineCountDiffers { m_count_msg, .. } => *m_count_msg,
            Evidence::AnswerTLineDiffers { t_line_msg, .. } => *t_line_msg,
            Evidence::AnswerMediaTypeMismatched { media_type_msg, .. } => *media_type_msg,
            Evidence::DirectionPairInvalid { direction_msg, .. } => *direction_msg,
            Evidence::RejectedStreamWithoutFormat { rejected_stream_msg, .. } => {
                *rejected_stream_msg
            }
            Evidence::ReOfferStreamsDropped { re_offer_msg, .. } => *re_offer_msg,
            Evidence::ZeroPortResurrected { zero_port_msg, .. } => *zero_port_msg,
            Evidence::PayloadTypeRemapped { payload_type_msg, .. } => *payload_type_msg,
            Evidence::RequiredHeaderAbsent { absent_msg, .. } => *absent_msg,
            Evidence::ForbiddenHeaderPresent { forbidden_msg, .. } => *forbidden_msg,
            Evidence::HeaderValueRejected { rejected_msg, .. } => *rejected_msg,
            Evidence::ResponseViaDiverged { via_msg, .. } => *via_msg,
            Evidence::ResponseCseqPhantom { phantom_msg, .. } => *phantom_msg,
            Evidence::DialogTagForeign { foreign_tag_msg, .. } => *foreign_tag_msg,
            Evidence::DialogRemoteTagForeign { foreign_from_msg, .. } => *foreign_from_msg,
            Evidence::PeerUriRewritten { peer_uri_msg, .. } => *peer_uri_msg,
            Evidence::DialogCallIdChanged { call_id_msg, .. } => *call_id_msg,
            Evidence::CancelUriDiverged { cancel_uri_msg, .. } => *cancel_uri_msg,
            Evidence::CancelBranchUnmatched { cancel_branch_msg, .. } => *cancel_branch_msg,
            Evidence::UasTagFlipped { tag_flip_msg, .. } => *tag_flip_msg,
            Evidence::Reliable1xx { reliable_1xx_msg, .. } => *reliable_1xx_msg,
            Evidence::SdpBodyRejected { sdp_body_msg, .. } => *sdp_body_msg,
            Evidence::HeldAndRejectedStreams { held_stream_msg, .. } => *held_stream_msg,
            Evidence::RungDiverged { rung_msg, .. } => *rung_msg,
        }
    }
}
