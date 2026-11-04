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

/// One reading with a fresh latch.
fn read(
    check: &FaceCheck,
    root: &std::path::Path,
    table: impl FnOnce() -> std::io::Result<Vec<Neighbour>>,
) -> FaceState {
    check.read_at(root, table, |_| {}, &mut FaceLatch::default())
}

/// Rewrite `eth1`'s operstate under `root`.
fn set_operstate(root: &std::path::Path, operstate: &str) {
    std::fs::write(root.join("eth1").join("operstate"), format!("{operstate}\n")).unwrap();
}

fn entry(addr: Ipv4Addr, state: u16) -> Neighbour {
    Neighbour { ifindex: IFINDEX, addr, state }
}

const REACHABLE: u16 = 0x02;

#[test]
fn an_up_interface_with_a_live_next_hop_is_usable() {
    let root = class_net("live", "up", None);
    let state = read(&check(&[TOR]), &root, || Ok(vec![entry(TOR, REACHABLE)]));
    assert_eq!(state, FaceState::Usable);
}

#[test]
fn a_missing_interface_is_not_usable() {
    let root = class_net("missing", "up", None);
    let state =
        read(&FaceCheck { interface: "eth9".into(), next_hops: vec![] }, &root, || Ok(vec![]));
    assert_eq!(state, FaceState::Missing);
}

#[test]
fn a_down_interface_is_not_usable() {
    let root = class_net("down", "down", None);
    let state = read(&check(&[TOR]), &root, || Ok(vec![entry(TOR, REACHABLE)]));
    assert_eq!(state, FaceState::Down("down".into()));
}

/// An interface that tracks no operstate is up while its carrier reads 1.
#[test]
fn an_unknown_operstate_follows_the_carrier() {
    let root = class_net("unknown-1", "unknown", Some("1"));
    assert!(read(&check(&[]), &root, || Ok(vec![])).is_usable());
    let root = class_net("unknown-0", "unknown", Some("0"));
    assert_eq!(read(&check(&[]), &root, || Ok(vec![])), FaceState::Down("unknown".into()));
}

#[test]
fn a_failed_or_incomplete_next_hop_makes_the_face_unusable() {
    let root = class_net("failed", "up", None);
    for state in [NUD_FAILED, NUD_INCOMPLETE] {
        let face = read(&check(&[TOR]), &root, || Ok(vec![entry(TOR, state)]));
        assert_eq!(face, FaceState::NextHopsUnresolved, "state {state:#x}");
    }
}

/// An entry flapping between INCOMPLETE and gone, as the kernel shows a next
/// hop that stopped answering, never reads usable: the latch holds until a
/// reading finds the next hop resolved (STALE here).
#[test]
fn an_incomplete_flap_stays_unusable_until_the_next_hop_resolves() {
    let root = class_net("flap", "up", None);
    let face = check(&[TOR]);
    let mut latch = FaceLatch::default();
    let mut step = |table: Vec<Neighbour>| face.read_at(&root, || Ok(table), |_| {}, &mut latch);
    assert_eq!(step(vec![entry(TOR, REACHABLE)]), FaceState::Usable);
    assert_eq!(step(vec![entry(TOR, NUD_INCOMPLETE)]), FaceState::NextHopsUnresolved);
    assert_eq!(step(vec![]), FaceState::NextHopsUnresolved, "gone after INCOMPLETE");
    assert_eq!(step(vec![entry(TOR, NUD_INCOMPLETE)]), FaceState::NextHopsUnresolved);
    assert_eq!(step(vec![]), FaceState::NextHopsUnresolved);
    assert_eq!(step(vec![entry(TOR, 0x04)]), FaceState::Usable, "STALE clears the latch");
    assert_eq!(step(vec![]), FaceState::Usable, "gone with no latch is not unresolved");
}

/// Every resolved state clears the latch; NOARP and no state do not.
#[test]
fn only_a_resolved_state_clears_the_latch() {
    let root = class_net("clears", "up", None);
    let face = check(&[TOR]);
    for resolved in [0x02, 0x04, 0x08, 0x10, 0x80] {
        let mut latch = FaceLatch::default();
        face.read_at(&root, || Ok(vec![entry(TOR, NUD_FAILED)]), |_| {}, &mut latch);
        let state = face.read_at(&root, || Ok(vec![entry(TOR, resolved)]), |_| {}, &mut latch);
        assert_eq!(state, FaceState::Usable, "state {resolved:#x}");
    }
    for other in [0x00, 0x40] {
        let mut latch = FaceLatch::default();
        face.read_at(&root, || Ok(vec![entry(TOR, NUD_FAILED)]), |_| {}, &mut latch);
        let state = face.read_at(&root, || Ok(vec![entry(TOR, other)]), |_| {}, &mut latch);
        assert_eq!(state, FaceState::NextHopsUnresolved, "state {other:#x}");
    }
}

/// While latched, every reading solicits every next hop: nothing else sends
/// toward them once the node is withdrawn. An answer clears the latch and the
/// soliciting with it; while none answers the latch holds, whatever the table
/// shows of an unanswered entry. A next hop is never solicited while no latch
/// is set: one dead hop of two is left to the traffic that still flows.
#[test]
fn the_latch_clears_on_the_solicits_answer_and_stays_while_none_answers() {
    let root = class_net("solicit", "up", None);
    let face = check(&[TOR, TOR_B]);
    let mut latch = FaceLatch::default();
    let mut step = |table: Vec<Neighbour>| {
        let mut asked = Vec::new();
        let state = face.read_at(&root, || Ok(table), |hop| asked.push(hop), &mut latch);
        (state, asked)
    };
    let usable = (FaceState::Usable, vec![]);
    let latched = (FaceState::NextHopsUnresolved, vec![TOR, TOR_B]);
    assert_eq!(step(vec![entry(TOR, REACHABLE), entry(TOR_B, REACHABLE)]), usable);
    assert_eq!(step(vec![entry(TOR, NUD_FAILED), entry(TOR_B, REACHABLE)]), usable);
    assert_eq!(step(vec![entry(TOR, NUD_FAILED), entry(TOR_B, NUD_FAILED)]), latched);
    assert_eq!(step(vec![]), latched, "no answer: the entries gone");
    assert_eq!(step(vec![entry(TOR, NUD_INCOMPLETE)]), latched, "no answer: the solicit pending");
    assert_eq!(step(vec![entry(TOR_B, NUD_FAILED)]), latched, "no answer: the solicit failed");
    assert_eq!(step(vec![entry(TOR_B, REACHABLE)]), usable, "one answer clears the latch");
    assert_eq!(step(vec![]), usable, "no latch, nothing solicited");
}

/// One reading of the interface down, between two up, holds the verdict: a
/// carrier flap shorter than the reading period withdraws nothing. The second
/// in a row is the face down, and an up reading starts the count again.
#[test]
fn one_down_reading_withdraws_nothing_and_two_do() {
    let root = class_net("two-down", "up", None);
    let face = check(&[TOR]);
    let mut latch = FaceLatch::default();
    let mut step = |operstate: &str| {
        set_operstate(&root, operstate);
        face.read_at(&root, || Ok(vec![entry(TOR, REACHABLE)]), |_| {}, &mut latch)
    };
    assert_eq!(step("up"), FaceState::Usable);
    assert!(step("down").is_usable(), "one down reading");
    assert_eq!(step("up"), FaceState::Usable);
    assert!(step("lowerlayerdown").is_usable(), "the count started again");
    assert_eq!(step("lowerlayerdown"), FaceState::Down("lowerlayerdown".into()));
    assert_eq!(step("down"), FaceState::Down("down".into()), "stays down");
    assert_eq!(step("up"), FaceState::Usable);
}

/// A face that read unusable has no verdict to hold: its first down reading is
/// down, so a proxy that starts on a dead carrier, or a latched face whose
/// carrier then drops, never reads usable for one reading.
#[test]
fn a_down_reading_holds_only_a_usable_face() {
    let root = class_net("down-unusable", "up", None);
    let face = check(&[TOR]);
    let mut latch = FaceLatch::default();
    let failed = || Ok(vec![entry(TOR, NUD_FAILED)]);
    assert_eq!(face.read_at(&root, failed, |_| {}, &mut latch), FaceState::NextHopsUnresolved);
    set_operstate(&root, "down");
    assert_eq!(face.read_at(&root, failed, |_| {}, &mut latch), FaceState::Down("down".into()));
}

/// An interface removed is no carrier flap: the first reading without it is
/// unusable.
#[test]
fn a_removed_interface_withdraws_at_its_first_reading() {
    let root = class_net("removed", "up", None);
    let face = check(&[TOR]);
    let mut latch = FaceLatch::default();
    let live = || Ok(vec![entry(TOR, REACHABLE)]);
    assert_eq!(face.read_at(&root, live, |_| {}, &mut latch), FaceState::Usable);
    std::fs::remove_dir_all(root.join("eth1")).unwrap();
    assert_eq!(face.read_at(&root, live, |_| {}, &mut latch), FaceState::Missing);
}

/// The monitor carries the first down reading to the next one, across its
/// blocking reads: one tick down keeps the face usable, the next withdraws it.
#[tokio::test(start_paused = true)]
async fn the_monitor_withdraws_on_the_second_down_reading() {
    let root = class_net("monitor-down", "up", None);
    let face = check(&[TOR]);
    let monitor = FaceMonitor::new(face.clone());
    let read_root = root.clone();
    let task = monitor.spawn_reading(
        std::time::Duration::from_millis(500),
        std::sync::Arc::new(move |latch: &mut FaceLatch| {
            face.read_at(&read_root, || Ok(vec![entry(TOR, REACHABLE)]), |_| {}, latch)
        }),
    );
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    assert!(monitor.usable());
    set_operstate(&root, "down");
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(monitor.usable(), "one reading down");
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(!monitor.usable(), "two readings down");
    task.abort();
}

/// The monitor publishes each reading, taken off the async runtime, and
/// reads unusable before its first one.
#[tokio::test(start_paused = true)]
async fn the_monitor_publishes_each_reading() {
    let monitor = FaceMonitor::new(check(&[TOR]));
    assert!(!monitor.usable(), "no reading yet");
    let usable = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let reads = usable.clone();
    let task = monitor.spawn_reading(
        std::time::Duration::from_millis(500),
        std::sync::Arc::new(move |_latch: &mut FaceLatch| {
            if reads.load(std::sync::atomic::Ordering::SeqCst) {
                FaceState::Usable
            } else {
                FaceState::Missing
            }
        }),
    );
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    assert!(monitor.usable());
    usable.store(false, std::sync::atomic::Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    assert!(!monitor.usable());
    task.abort();
}

/// One live next hop of two carries the face.
#[test]
fn one_live_next_hop_of_two_keeps_the_face_usable() {
    let root = class_net("one-of-two", "up", None);
    let table = vec![entry(TOR, NUD_FAILED), entry(TOR_B, REACHABLE)];
    assert!(read(&check(&[TOR, TOR_B]), &root, || Ok(table)).is_usable());
}

/// A next hop the table does not hold, or holds as FAILED on another
/// interface, is not FAILED here: nothing is probed.
#[test]
fn an_absent_next_hop_or_one_failed_elsewhere_is_not_unresolved() {
    let root = class_net("absent", "up", None);
    assert!(read(&check(&[TOR]), &root, || Ok(vec![])).is_usable());
    let elsewhere = Neighbour { ifindex: IFINDEX + 1, addr: TOR, state: NUD_FAILED };
    assert!(read(&check(&[TOR]), &root, || Ok(vec![elsewhere])).is_usable());
}

#[test]
fn an_unreadable_neighbour_table_is_not_usable() {
    let root = class_net("unreadable", "up", None);
    let state = read(&check(&[TOR]), &root, || Err(std::io::Error::other("denied")));
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
