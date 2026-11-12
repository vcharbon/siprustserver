use super::*;
use pivot_schema::call::{Abandoned, Attempt, Call, Callee, Refused};

fn call(refused: Option<Refused>, abandoned: Option<Abandoned>, dials: bool) -> Call {
    Call {
        id: "c1".into(),
        caller_leg: "A".into(),
        attempts: if dials {
            vec![Attempt {
                branch: 0,
                position: 0,
                leg: "B".into(),
                callee: Callee { identity: "called-0-0".into() },
                r#final: None,
                cause: None,
                joined_by: None,
                cause_evidence: Vec::new(),
                join_evidence: None,
                no_answer_ms: None,
            }]
        } else {
            Vec::new()
        },
        refused,
        abandoned,
        relay18x: None,
    }
}

fn refusal() -> Refused {
    Refused { step: "s3".into(), evidence: None }
}

fn abandon() -> Abandoned {
    Abandoned { step: "s3".into(), evidence: None }
}

fn addrs() -> BTreeMap<String, SocketAddr> {
    BTreeMap::from([("ep0".to_string(), "127.0.0.1:5060".parse().unwrap())])
}

#[test]
fn a_document_that_dials_states_no_reason_not_to() {
    assert_eq!(NoDial::of(&[call(None, None, true)]), None);
    assert_eq!(NoDial::of(&[call(Some(refusal()), None, false)]), Some(NoDial::Refused));
    assert_eq!(NoDial::of(&[call(None, Some(abandon()), false)]), Some(NoDial::Abandoned));
}

/// The system still dials an abandoned case, so its egress must land on a
/// socket nothing binds, never on the caller's, which the document plays.
#[test]
fn an_abandoned_case_egresses_where_nothing_binds() {
    let target = egress_target(&addrs(), "ep1", Some(NoDial::Abandoned)).unwrap();
    assert_eq!(target, "127.0.0.1:9".parse::<SocketAddr>().unwrap());
    let named = egress_target(&addrs(), "ep0", Some(NoDial::Abandoned)).unwrap();
    assert_eq!(named, "127.0.0.1:9".parse::<SocketAddr>().unwrap());
}

#[test]
fn a_refused_case_still_stands_its_own_first_endpoint_in() {
    let target = egress_target(&addrs(), "ep1", Some(NoDial::Refused)).unwrap();
    assert_eq!(target, "127.0.0.1:5060".parse::<SocketAddr>().unwrap());
}

#[test]
fn a_case_that_dials_takes_the_endpoint_it_names_and_refuses_any_other() {
    assert_eq!(
        egress_target(&addrs(), "ep0", None).unwrap(),
        "127.0.0.1:5060".parse::<SocketAddr>().unwrap()
    );
    assert!(egress_target(&addrs(), "ep1", None).is_err());
}
