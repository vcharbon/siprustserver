//! The decision core's turn lane: the caller's INVITE dialled and the turns
//! run over the call ([`drive`]), the decisions an INVITE is routed under
//! ([`invite`]), and the scripts every case starts from ([`scripts`]).

#[path = "../../../crates/b2bua-core/tests/it/turn_lane/drive.rs"]
pub mod drive;
#[path = "../../../crates/b2bua-core/tests/it/turn_lane/invite.rs"]
pub mod invite;
#[path = "../../../crates/b2bua-core/tests/it/turn_lane/scripts.rs"]
pub mod scripts;
