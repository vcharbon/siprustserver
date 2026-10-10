use metric_catalogue::{DEFAULT_CAP, OVERFLOW};

use super::*;
use crate::catalogue::RETRANSMITS;

fn extension(token: &str) -> MethodSlot {
    method_slot(&Method::Other(token.to_owned()))
}

/// A send error is counted unless it is a full send buffer, which the
/// endpoint counts as would-block.
#[test]
fn a_send_error_is_counted_unless_it_would_block() {
    let inner = MetricsInner::new();
    let refused = |kind| Err(sip_net::SendError { message: String::new(), kind });
    inner.count_send(Ok(()));
    inner.count_send(refused(sip_net::SendErrorKind::WouldBlock));
    assert_eq!(inner.send_errors.load(Ordering::Relaxed), 0);
    inner.count_send(refused(sip_net::SendErrorKind::Other));
    inner.count_send(refused(sip_net::SendErrorKind::Unreachable));
    assert_eq!(inner.send_errors.load(Ordering::Relaxed), 2);
}

/// An extension method's rung is counted under its own token, never under
/// a shared bucket.
#[test]
fn an_extension_method_keeps_its_own_row() {
    let family = RetransmitFamily::new();
    family.record_request(Class::NonInviteClient, extension("FOO"));
    family.record_request(Class::NonInviteClient, extension("FOO"));
    family.record_trigger(&Method::Other("BAR".to_owned()), 200);
    family.record_request(Class::NonInviteClient, method_slot(&Method::Bye));
    let rows = family.rows();
    let row = |method: &str, code| {
        rows.iter().find(|r| r.method == method && r.code == code).map(|r| (r.ladder, r.count))
    };
    assert_eq!(row("FOO", None), Some(("non-invite-client", 2)));
    assert_eq!(row("BAR", Some(200)), Some(("trigger", 1)));
    assert_eq!(row("BYE", None), Some(("non-invite-client", 1)));
    assert_eq!(family.total(Class::NonInviteClient), 3);
    assert_eq!(family.triggered(), 1);
}

/// Past the cap an extension method's rung lands on the overflow row and
/// is counted there.
#[test]
fn past_the_cap_an_extension_rung_lands_on_the_overflow_row() {
    let family = RetransmitFamily::new();
    assert_eq!(RETRANSMITS.cap().map(|c| c.max), Some(DEFAULT_CAP));
    for i in 0..DEFAULT_CAP + 3 {
        family.record_request(Class::NonInviteClient, extension(&format!("X{i}")));
    }
    let rows = family.rows();
    assert_eq!(rows.iter().filter(|r| r.method.starts_with('X')).count(), DEFAULT_CAP);
    let overflow = rows.iter().find(|r| r.method == OVERFLOW).expect("overflow row");
    assert_eq!((overflow.ladder, overflow.count), ("non-invite-client", 3));
    assert_eq!(family.overflowed(), 3);
}
