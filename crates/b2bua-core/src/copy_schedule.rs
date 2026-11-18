//! Whether a peer's copy of an INVITE final is owed its ACK again (ADR-0057
//! §10, ADR-0063 §1): an answerer re-sends a 2xx on §13.3.1.4's ladder and a
//! non-2xx on Timer G's (RFC 3261 §17.2.1), so the copies up to that ladder's
//! re-sends are answered at any pace (a duplicate, one crossing the ACK) and a
//! later one only T1 after the last answered one, as a peer's larger T1 spaces
//! them (§17.1.1.1). A peer answering each ACK with its final again stops
//! there.

use std::time::Duration;

use sip_retransmit::Class;

/// The class whose ladder re-sends an INVITE final of `status`: a 2xx's
/// (§13.3.1.4), else a non-2xx's (§17.2.1).
pub fn class_of(status: u16) -> Class {
    if (200..300).contains(&status) {
        Class::Final2xx
    } else {
        Class::InviteServerFinal
    }
}

/// The `kind` label a copy of an INVITE final of `class` absorbed by a call
/// counts under on `b2bua_copies_past_schedule_total`.
pub fn kind(class: Class) -> &'static str {
    match class {
        Class::Final2xx => "2xx",
        _ => "non-2xx",
    }
}

/// Whether `copy`, the copy's number among those after the final's first
/// arrival (from 1), of a final re-sent on `class`'s ladder, is owed its ACK
/// ([`Class::owes_copy`]): `since_answered_ms` since the last copy answered
/// (`None`: none was), a negative span read as zero.
pub fn owed(class: Class, copy: u32, since_answered_ms: Option<i64>) -> bool {
    let since = since_answered_ms.map(|s| Duration::from_millis(u64::try_from(s).unwrap_or(0)));
    class.owes_copy(copy, since)
}

#[cfg(test)]
mod tests {
    use sip_retransmit::timers::T1;

    use super::*;

    #[test]
    fn the_ladders_re_sends_are_owed_at_any_pace() {
        for class in [Class::Final2xx, Class::InviteServerFinal] {
            assert!(owed(class, class.resends(), Some(0)), "{class:?}: the last re-send");
            assert!(!owed(class, class.resends() + 1, Some(0)), "{class:?}: one past it");
        }
    }

    #[test]
    fn a_copy_past_the_ladder_is_owed_t1_after_the_last_answered() {
        let past = Class::Final2xx.resends() + 1;
        let t1 = i64::try_from(T1).expect("T1 fits");
        assert!(!owed(Class::Final2xx, past, Some(t1 - 1)), "inside T1");
        assert!(owed(Class::Final2xx, past, Some(t1)), "T1 after");
        assert!(owed(Class::Final2xx, past, None), "none answered yet");
    }

    #[test]
    fn a_final_is_re_sent_on_its_own_ladder() {
        assert_eq!(class_of(200), Class::Final2xx);
        assert_eq!(class_of(488), Class::InviteServerFinal);
        assert_eq!(kind(class_of(299)), "2xx");
        assert_eq!(kind(class_of(300)), "non-2xx");
    }
}
