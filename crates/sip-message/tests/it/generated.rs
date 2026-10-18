//! The generators as the tests call them: a message that cannot be built fails
//! the test with its reason.

use sip_message::generators::{
    self, GenerateAckFor2xxOpts, GenerateInDialogRequestOpts, GenerateOutOfDialogRequestOpts,
    GenerateRelayedResponseOpts, GenerateResponseOpts, InDialogMethod, InDialogResult,
    InviteClientTransactionHandle, OutOfDialogMethod, StackDialog,
};
use sip_message::{SipHeader, SipRequest, SipResponse};

fn built<T>(made: Result<T, generators::GenerateError>) -> T {
    made.unwrap_or_else(|e| panic!("{e}"))
}

pub fn generate_response(
    request: &SipRequest,
    status: u16,
    reason: &str,
    opts: &GenerateResponseOpts,
) -> SipResponse {
    built(generators::generate_response(request, status, reason, opts))
}

pub fn generate_relayed_response(
    status: u16,
    reason: &str,
    opts: &GenerateRelayedResponseOpts,
) -> SipResponse {
    built(generators::generate_relayed_response(status, reason, opts))
}

pub fn generate_cancel(invite: &InviteClientTransactionHandle, extra: &[SipHeader]) -> SipRequest {
    built(generators::generate_cancel(invite, extra))
}

pub fn generate_ack_for_2xx(
    invite: Option<&InviteClientTransactionHandle>,
    dialog: &StackDialog,
    opts: &GenerateAckFor2xxOpts,
) -> SipRequest {
    built(generators::generate_ack_for_2xx(invite, dialog, opts))
}

pub fn generate_ack_for_non_2xx(
    invite: &SipRequest,
    final_response: &SipResponse,
    extra: &[SipHeader],
) -> SipRequest {
    built(generators::generate_ack_for_non_2xx(invite, final_response, extra))
}

pub fn generate_ack_for_non_2xx_in_dialog(
    dialog: &StackDialog,
    final_response: &SipResponse,
) -> SipRequest {
    built(generators::generate_ack_for_non_2xx_in_dialog(dialog, final_response))
}

pub fn generate_in_dialog_request(
    method: InDialogMethod,
    dialog: &StackDialog,
    opts: &GenerateInDialogRequestOpts,
) -> InDialogResult {
    built(generators::generate_in_dialog_request(method, dialog, opts))
}

pub fn generate_out_of_dialog_request(
    method: OutOfDialogMethod,
    opts: &GenerateOutOfDialogRequestOpts,
) -> SipRequest {
    built(generators::generate_out_of_dialog_request(method, opts))
}
