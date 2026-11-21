//! The command funnel's dispatch: each [`Command`] a handle sent, run on the
//! owner and answered on its reply channel.

use sip_net::UdpEndpoint;

use super::call_txns::CallTxns;
use super::handle::Command;
use super::owner::Owner;

impl Owner {
    pub(super) async fn handle_command(&mut self, endpoint: &dyn UdpEndpoint, cmd: Command) {
        match cmd {
            Command::SendRequest { msg, dest, txn_type, reply } => {
                let handle = self.do_send_request(endpoint, *msg, dest, txn_type).await;
                let _ = reply.send(handle);
            }
            Command::SendResponse { msg, dest, reply } => {
                let sent = self.do_send_response(endpoint, *msg, dest).await;
                let _ = reply.send(sent);
            }
            Command::SendRaw { call_id, buf, dest, reply } => {
                self.send_buffer(endpoint, &call_id, &buf, dest).await;
                let _ = reply.send(());
            }
            Command::Seed { call_ref, seeds, reply } => {
                let _ = reply.send(self.do_seed(endpoint, &call_ref, seeds).await);
            }
            Command::Reoffer { message, src, reply } => {
                let disposition = self.do_reoffer(endpoint, *message, src).await;
                let _ = reply.send(disposition);
            }
            Command::CancelTxnsForCall { call_ref, reply } => {
                self.do_cancel_txns_for_call(&call_ref);
                let _ = reply.send(());
            }
            Command::RelinquishCall { call_ref, reply } => {
                self.relinquish_call(endpoint, &call_ref, reply).await
            }
            Command::RelinquishServer { key, reply } => self.relinquish_server(&key, reply),
            Command::RelinquishUnansweredServer { key, reply } => {
                self.relinquish_unanswered_server(&key, reply)
            }
            Command::AttachServerToCall { key, call_ref } => {
                self.attach_server_to_call(&key, &call_ref)
            }
            Command::RelinquishClient { branch, reply } => {
                self.relinquish_client(endpoint, &branch, reply).await
            }
            Command::RelinquishAccepted { call_ref, reply } => {
                let _ = reply.send(self.relinquish_accepted_of_call(&call_ref));
            }
            Command::ForgetUnansweredOfCall { call_ref, reply } => {
                let _ = reply.send(self.forget_unanswered_of_call(&call_ref));
            }
            Command::AnswerUnansweredInvitesOfCall { call_ref, status, reason, reply } => {
                let answered = self
                    .answer_unanswered_invites_of_call(endpoint, &call_ref, status, &reason)
                    .await;
                let _ = reply.send(answered);
            }
            Command::ActiveTxnCount { call_ref, call_ids, reply } => {
                let on_books = self.txn_index.get(call_ref.as_str()).map_or(0, CallTxns::on_books);
                let _ = reply.send(on_books + self.unattributed_serving(&call_ids));
            }
            Command::HoldsClient { branch, reply } => {
                let _ = reply.send(self.clients.contains_key(&branch));
            }
            Command::ReleaseCancel { call_id, from_tag, cseq, invite, reply } => {
                let key = (call_id, from_tag, cseq);
                let _ = reply.send(self.release_held_cancel(endpoint, &key, invite).await);
            }
            Command::ReleaseAck { branch, reply } => {
                let _ = reply.send(self.send_held_ack(endpoint, &branch).await);
            }
            Command::ReleaseTrying { key, reply } => {
                let _ = reply.send(self.send_first_trying(endpoint, key.id()).await);
            }
            Command::WatchSelfRelease { call_ref, call_ids, reply } => {
                self.watch_self_release(call_ref, call_ids);
                let _ = reply.send(());
            }
        }
    }
}
