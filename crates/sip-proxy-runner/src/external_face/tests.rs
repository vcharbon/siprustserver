//! The external face read from a scripted `/sys/class/net` directory and a
//! scripted neighbour table, and the rtnetlink answer parsed from bytes.

use std::net::Ipv4Addr;
use std::path::PathBuf;

use super::neighbours::{parse, request, Parsed};
use super::*;

const IFINDEX: i32 = 7;
const TOR: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 1);
const TOR_B: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 2);

/// A `/sys/class/net` holding `eth1` with `operstate` (and `carrier` when
/// given), under a directory of its own.
fn class_net(test: &str, operstate: &str, carrier: Option<&str>) -> PathBuf {
    let root = std::env::temp_dir().join(format!("ext-face-{}-{test}", std::process::id()));
    let dir = root.join("eth1");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("operstate"), format!("{operstate}\n")).unwrap();
    std::fs::write(dir.join("ifindex"), format!("{IFINDEX}\n")).unwrap();
    if let Some(c) = carrier {
        std::fs::write(dir.join("carrier"), format!("{c}\n")).unwrap();
    }
    root
}

fn check(next_hops: &[Ipv4Addr]) -> FaceCheck {
    FaceCheck { interface: "eth1".into(), next_hops: next_hops.to_vec() }
}

fn entry(addr: Ipv4Addr, state: u16) -> Neighbour {
    Neighbour { ifindex: IFINDEX, addr, state }
}

const REACHABLE: u16 = 0x02;

#[test]
fn an_up_interface_with_a_live_next_hop_is_usable() {
    let root = class_net("live", "up", None);
    let state = check(&[TOR]).read_at(&root, || Ok(vec![entry(TOR, REACHABLE)]));
    assert_eq!(state, FaceState::Usable);
}

#[test]
fn a_missing_interface_is_not_usable() {
    let root = class_net("missing", "up", None);
    let state =
        FaceCheck { interface: "eth9".into(), next_hops: vec![] }.read_at(&root, || Ok(vec![]));
    assert_eq!(state, FaceState::Missing);
}

#[test]
fn a_down_interface_is_not_usable() {
    let root = class_net("down", "down", None);
    let state = check(&[TOR]).read_at(&root, || Ok(vec![entry(TOR, REACHABLE)]));
    assert_eq!(state, FaceState::Down("down".into()));
}

/// An interface that tracks no operstate is up while its carrier reads 1.
#[test]
fn an_unknown_operstate_follows_the_carrier() {
    let root = class_net("unknown-1", "unknown", Some("1"));
    assert!(check(&[]).read_at(&root, || Ok(vec![])).is_usable());
    let root = class_net("unknown-0", "unknown", Some("0"));
    assert_eq!(check(&[]).read_at(&root, || Ok(vec![])), FaceState::Down("unknown".into()));
}

#[test]
fn a_failed_next_hop_makes_the_face_unusable() {
    let root = class_net("failed", "up", None);
    let state = check(&[TOR]).read_at(&root, || Ok(vec![entry(TOR, NUD_FAILED)]));
    assert_eq!(state, FaceState::NextHopsFailed);
}

/// One live next hop of two carries the face.
#[test]
fn one_live_next_hop_of_two_keeps_the_face_usable() {
    let root = class_net("one-of-two", "up", None);
    let table = vec![entry(TOR, NUD_FAILED), entry(TOR_B, REACHABLE)];
    assert!(check(&[TOR, TOR_B]).read_at(&root, || Ok(table)).is_usable());
}

/// A next hop the table does not hold, or holds as FAILED on another
/// interface, is not FAILED here: nothing is probed.
#[test]
fn an_absent_next_hop_or_one_failed_elsewhere_is_not_failed() {
    let root = class_net("absent", "up", None);
    assert!(check(&[TOR]).read_at(&root, || Ok(vec![])).is_usable());
    let elsewhere = Neighbour { ifindex: IFINDEX + 1, addr: TOR, state: NUD_FAILED };
    assert!(check(&[TOR]).read_at(&root, || Ok(vec![elsewhere])).is_usable());
}

#[test]
fn an_unreadable_neighbour_table_is_not_usable() {
    let root = class_net("unreadable", "up", None);
    let state = check(&[TOR]).read_at(&root, || Err(std::io::Error::other("denied")));
    assert!(matches!(state, FaceState::Unreadable(_)), "{state:?}");
}

#[test]
fn interface_names_are_checked_before_any_path_is_built() {
    for good in ["eth1", "bond0.100", "enp1s0f1"] {
        assert!(valid_interface_name(good), "{good}");
    }
    for bad in ["", ".", "..", "../etc", "eth 1", "a234567890123456"] {
        assert!(!valid_interface_name(bad), "{bad}");
    }
}

/// One `RTM_NEWNEIGH` message for `addr` on `ifindex` in `state`, with an
/// `NDA_LLADDR` attribute before the `NDA_DST` one.
fn newneigh(ifindex: i32, addr: Ipv4Addr, state: u16) -> Vec<u8> {
    let mut payload = vec![2u8, 0, 0, 0];
    payload.extend_from_slice(&ifindex.to_ne_bytes());
    payload.extend_from_slice(&state.to_ne_bytes());
    payload.extend_from_slice(&[0, 1]);
    // NDA_LLADDR: 6 bytes, padded to 8.
    payload.extend_from_slice(&10u16.to_ne_bytes());
    payload.extend_from_slice(&2u16.to_ne_bytes());
    payload.extend_from_slice(&[2, 0, 0, 0, 0, 1, 0, 0]);
    payload.extend_from_slice(&8u16.to_ne_bytes());
    payload.extend_from_slice(&1u16.to_ne_bytes());
    payload.extend_from_slice(&addr.octets());
    message(28, &payload)
}

fn message(kind: u16, payload: &[u8]) -> Vec<u8> {
    let len = u32::try_from(16 + payload.len()).unwrap();
    let mut out = len.to_ne_bytes().to_vec();
    out.extend_from_slice(&kind.to_ne_bytes());
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&[0u8; 8]);
    out.extend_from_slice(payload);
    while out.len() % 4 != 0 {
        out.push(0);
    }
    out
}

#[test]
fn the_dump_answer_is_parsed_across_chunks() {
    let mut first = newneigh(IFINDEX, TOR, NUD_FAILED);
    first.extend(newneigh(3, Ipv4Addr::new(10, 0, 0, 1), REACHABLE));
    let mut out = Vec::new();
    assert_eq!(parse(&first, &mut out).unwrap(), Parsed::More);
    let done = message(3, &0i32.to_ne_bytes());
    assert_eq!(parse(&done, &mut out).unwrap(), Parsed::Done);
    assert_eq!(
        out,
        vec![
            entry(TOR, NUD_FAILED),
            Neighbour { ifindex: 3, addr: Ipv4Addr::new(10, 0, 0, 1), state: REACHABLE }
        ]
    );
}

#[test]
fn a_kernel_error_answer_is_an_error() {
    let mut out = Vec::new();
    let err = parse(&message(2, &(-13i32).to_ne_bytes()), &mut out).unwrap_err();
    assert_eq!(err.raw_os_error(), Some(13));
}

/// Every truncation of a valid answer is an error or a shorter parse, never
/// a panic (ADR-0051).
#[test]
fn a_truncated_answer_never_panics() {
    let whole = newneigh(IFINDEX, TOR, REACHABLE);
    for cut in 0..whole.len() {
        let mut out = Vec::new();
        let _ = parse(whole.get(..cut).unwrap(), &mut out);
    }
    let mut lying = whole.clone();
    lying[0] = 0xff;
    assert!(parse(&lying, &mut Vec::new()).is_err());
}

#[test]
fn the_request_asks_a_dump_of_the_ipv4_table() {
    let req = request(9);
    assert_eq!(req.len(), 28);
    assert_eq!(u32::from_ne_bytes(req[0..4].try_into().unwrap()), 28);
    assert_eq!(u16::from_ne_bytes(req[4..6].try_into().unwrap()), 30);
    assert_eq!(u16::from_ne_bytes(req[6..8].try_into().unwrap()), 0x301);
    assert_eq!(req[16], 2);
}

/// The running kernel answers the dump.
#[test]
fn the_running_kernel_answers_the_dump() {
    dump_ipv4().expect("the neighbour table is readable");
}
