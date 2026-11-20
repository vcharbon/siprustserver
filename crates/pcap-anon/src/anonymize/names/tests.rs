//! The whole anonymisation on a synthetic two-leg call: an application server
//! re-originating under a derived Call-ID, every non-number identity a
//! production capture carries placed where it sits on the wire, in every
//! spelling the review of the names pass found to leak. Every value is
//! invented.

use serde_json::{json, Value};

use crate::anonymize::{anonymize_in, parse_ok_map};
use crate::cut::Cut;
use crate::fake::address::Scope;
use crate::fake::Seed;
use crate::model::{ingest_str, Flows};

const PEER: &str = "8.8.8.3:5060";
const SUT: &str = "172.16.245.246:5060";
const CALL_ID: &str = "a84b4c76e66710@pcscf1.ims.alpha-carrier.zz";

/// Every invented value the call carries outside its numbers.
const NAMES: &[&str] = &[
    "alpha",
    "pcscf1",
    "Fakevendor",
    "Fakecarrier",
    "8.8.8.3",
    "8.8.8.4",
    "8.8.8.5",
    "8-8-8-200",
    "8.8.8.202",
    "3fff:cb0:1234::5",
    "3fff:e0a:1:2::7",
    "3fff:6d0::9",
    "0A1B2C3D4E5F6071",
    "media.fakevendor-tunes",
    "kappa-ims",
    "Фейкова",
    "rhodium",
    "Frobnitz",
];

fn invite() -> String {
    let sdp = "v=0\r\no=rhodium 1 1 IN IP4 8.8.8.3\r\ns=Frobnitz Bridge\r\nc=IN IP4 8.8.8.5\r\n\
               t=0 0\r\nm=audio 4000 RTP/AVP 0\r\n\
               a=candidate:1 1 UDP 2130706431 3fff:e0a:1:2::7 9000 typ host\r\n";
    format!(
        "INVITE sip:+2862345678@as.ims.alpha-carrier.zz;user=phone SIP/2.0\r\n\
         Via: SIP/2.0/UDP 8.8.8.3:5060;branch=z9hG4bK1;received=8.8.8.4\r\n\
         Via: SIP/2.0/UDP pcscf1.ims.alpha-carrier.zz:5060;branch=z9hG4bK2\r\n\
         Via: SIP/2.0/UDP host-85-12-34-200.dyn.alpha-isp.zz:5060;branch=z9hG4bK3;received=3fff:6d0::9\r\n\
         Via: SIP/2.0/UDP Core.Alpha-Carrier.ZZ:5060;branch=z9hG4bK4\r\n\
         Record-Route: <sip:edge.alpha-carrier.zz;lr>\r\n\
         From: <sip:+2862345678@ims.alpha-carrier.zz>;tag=f1\r\n\
         To: <sip:+2868765432@ims.alpha-carrier.zz>\r\n\
         P-Preferred-Identity: <tel:+2862345678;phone-context=ims.kappa-ims.zz>\r\n\
         Call-ID: {CALL_ID}\r\nCSeq: 1 INVITE\r\n\
         Contact: <sip:+2862345678@[3fff:cb0:1234::5]:5060>\r\n\
         Call-Info: <http://media.fakevendor-tunes.zz/ring.wav>;purpose=info\r\n\
         Warning: 399 core.alpha-carrier.zz \"busy\"\r\n\
         X-Origin: [::ffff:8.8.8.202]\r\nX-Peer-V6: 3fff:e0a:1:2::7\r\n\
         P-Charging-Vector: icid-value=0A1B2C3D4E5F6071;icid-generated-at=8.8.8.3;orig-ioi=ims.alpha-carrier.zz\r\n\
         Authorization: Digest username=\"u\", realm=\"ims.alpha-carrier.zz\", nonce=\"n\", \
         uri=\"sip:as.ims.alpha-carrier.zz\", response=\"r\"\r\n\
         User-Agent: Fakevendor-SBC/7.2.1\r\nOrganization: Fakecarrier Telecom\r\nSubject: Фейкова\r\n\
         Content-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{sdp}",
        sdp.len()
    )
}

fn answer() -> String {
    format!(
        "SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 172.16.245.246:5060;branch=z9hG4bK3\r\n\
         From: <sip:+2862345678@ims.alpha-carrier.zz>;tag=f1\r\n\
         To: <sip:+2868765432@ims.alpha-carrier.zz>;tag=t1\r\n\
         Call-ID: 1-{CALL_ID}\r\nCSeq: 1 INVITE\r\n\
         P-Charging-Vector: icid-value=0A1B2C3D4E5F6071;term-ioi=ims.alpha-carrier.zz\r\n\
         Server: Fakevendor-AS 3.1\r\nContent-Length: 0\r\n\r\n"
    )
}

fn party(uri: &str, tag: Option<&str>) -> Value {
    json!({ "uri": uri, "tag": tag })
}

fn leg(call_id: &str, raw: String, summary: Value, src: &str, dst: &str) -> Value {
    json!({
        "call_id": call_id,
        "hops": [{ "a": src, "b": dst }],
        "invite": null, "final_status": null, "saw_180": false, "terminated_by": null,
        "tokens": ["0A1B2C3D4E5F6071"],
        "msgs": [{ "ts_us": 0, "src": src, "dst": dst, "hop": 0, "retx": false, "raw": raw,
                   "summary": summary }]
    })
}

/// The call: the caller's INVITE on the a-leg, the callee's 200 on the derived
/// b-leg, grouped on the charging id and on the derivation.
fn call() -> Flows {
    let from = party("sip:+2862345678@ims.alpha-carrier.zz", Some("f1"));
    let to = party("sip:+2868765432@ims.alpha-carrier.zz", None);
    let cseq = json!({ "seq": 1, "method": "INVITE" });
    let doc = json!({
        "schema": crate::model::SUPPORTED_SCHEMA,
        "decode_stats": { "records": 0, "non_ip": 0, "non_udp": 0, "snap_truncated": 0,
                          "datagrams": 0, "fragments": 0, "reassembled": 0, "frag_dropped": 0,
                          "tail_truncated": 0 },
        "flow_stats": { "sip_messages": 2, "capture_dups": 0, "parse_failed": 0, "non_sip": 0 },
        "legs": [
            leg(CALL_ID, invite(), json!({ "kind": "request", "method": "INVITE",
                "uri": "sip:+2862345678@as.ims.alpha-carrier.zz;user=phone", "cseq": cseq,
                "from": from, "to": to }), PEER, SUT),
            leg(&format!("1-{CALL_ID}"), answer(), json!({ "kind": "response", "status": 200,
                "reason": "OK", "cseq": cseq, "from": from,
                "to": party("sip:+2868765432@ims.alpha-carrier.zz", Some("t1")) }), PEER, SUT),
        ],
        "groups": [{ "legs": [0, 1], "evidence": [
            { "kind": "shared_header_param", "strategy": 1, "header": "P-Charging-Vector",
              "param": "icid-value", "token": "0A1B2C3D4E5F6071", "legs": [0, 1] },
            { "kind": "derived_call_id", "strategy": 2, "legs": [0, 1], "prefix": "1-",
              "as_socket": SUT, "peer_socket": PEER, "shared_hop": true, "dt_us": 0 }
        ] }]
    });
    ingest_str(&doc.to_string()).expect("the synthetic call ingests")
}

fn raws(flows: &Flows) -> Vec<Vec<u8>> {
    flows.legs.iter().flat_map(|l| l.msgs.iter().map(crate::model::wire)).collect()
}

fn text(flows: &Flows) -> String {
    let model = flows
        .legs
        .iter()
        .flat_map(|l| l.msgs.iter().flat_map(|m| [m.src.clone(), m.dst.clone()]))
        .chain(flows.legs.iter().flat_map(|l| l.tokens.clone()))
        .collect::<Vec<_>>()
        .join(" ");
    let wire: Vec<String> =
        raws(flows).iter().map(|r| String::from_utf8_lossy(r).into_owned()).collect();
    format!("{model}\n{}", wire.join("\n"))
}

/// The value of `name=` in a datagram (up to `;`, `,` or the line end).
fn param(raw: &[u8], name: &str) -> String {
    let text = String::from_utf8_lossy(raw);
    let at = text.find(&format!("{name}=")).expect("the parameter") + name.len() + 1;
    text[at..].split([';', ',', '\r']).next().expect("a value").to_string()
}

#[test]
fn every_name_of_the_call_maps_and_every_datagram_keeps_its_length() {
    let mut flows = call();
    let before = raws(&flows);
    let parsed = parse_ok_map(&flows, &crate::testflow::reading());
    let report = anonymize_in(
        &mut flows,
        &crate::testflow::plan(),
        &Seed::from_bytes([1; 32]),
        &Scope::default(),
    )
    .expect("anonymises");
    let after = text(&flows).to_ascii_lowercase();
    for name in NAMES {
        assert!(!after.contains(&name.to_ascii_lowercase()), "{name} survives the pass");
    }
    for (b, a) in before.iter().zip(raws(&flows)) {
        assert_eq!(b.len(), a.len(), "a datagram keeps its byte length");
    }
    assert_eq!(
        parse_ok_map(&flows, &crate::testflow::reading()),
        parsed,
        "every datagram still parses"
    );
    assert!(after.contains("172.16.245.246"), "a private address stays");
    assert_eq!(
        flows.legs[1].call_id,
        format!("1-{}", flows.legs[0].call_id),
        "the derivation holds"
    );
    assert_eq!(
        param(&crate::model::wire(&flows.legs[0].msgs[0]), "icid-value"),
        param(&crate::model::wire(&flows.legs[1].msgs[0]), "icid-value"),
        "the legs still share their charging id"
    );
    assert_eq!(
        flows.legs[0].tokens[0],
        param(&crate::model::wire(&flows.legs[0].msgs[0]), "icid-value")
    );
    let names = super::NameCounts::of(&report.names);
    assert!(
        names.hosts >= 8 && names.addresses >= 7 && names.icids == 1 && names.texts >= 5,
        "{names:?}"
    );
}

#[test]
fn the_raw_call_under_one_seed_reproduces_byte_for_byte_and_its_output_is_refused() {
    let plan = crate::testflow::plan();
    let (mut flows, mut again) = (call(), call());
    anonymize_in(&mut flows, &plan, &Seed::from_bytes([1; 32]), &Scope::default()).expect("pass 1");
    anonymize_in(&mut again, &plan, &Seed::from_bytes([1; 32]), &Scope::default()).expect("again");
    assert_eq!(text(&again), text(&flows));
    let refused = anonymize_in(&mut flows, &plan, &Seed::from_bytes([2; 32]), &Scope::default());
    assert!(matches!(refused, Err(crate::anonymize::AnonymizeError::AlreadyRewritten(_))));
}

#[test]
fn verify_counts_the_names_that_survive_and_none_after_the_pass() {
    let (plan, scope) = (crate::testflow::plan(), Scope::default());
    let raw = call();
    let survivors = crate::verify::names::name_survivors(&raw, &call(), &scope);
    assert!(survivors >= 8, "an untouched document leaks every name: {survivors}");
    let mut anonymised = call();
    let report = anonymize_in(&mut anonymised, &plan, &Seed::from_bytes([1; 32]), &scope)
        .expect("anonymises");
    assert_eq!(crate::verify::names::name_survivors(&raw, &anonymised, &scope), 0);
    let reading = crate::testflow::reading();
    let leaks = crate::verify::leaks::leaks(&anonymised, &scope, &report.fakes, &reading);
    assert_eq!(leaks.total(), 0, "{leaks:?}");
    let untouched = crate::verify::leaks::leaks(&call(), &scope, &report.fakes, &reading);
    assert!(untouched.total() >= 8, "{untouched:?}");
}

#[test]
fn the_addresses_a_scope_keeps_stay_and_all_addresses_maps_the_private_ones() {
    let plan = crate::testflow::plan();
    let cut = Cut::from_json_str(r#"{"sut":["8.8.8.0/24"]}"#).expect("a cut");
    let mut kept = call();
    anonymize_in(&mut kept, &plan, &Seed::from_bytes([1; 32]), &Scope::default().keeping(&cut))
        .expect("anonymises");
    assert!(text(&kept).contains("8.8.8.3"), "the stated SUT stays");
    assert!(!text(&kept).contains("alpha"));
    let mut whole = call();
    anonymize_in(&mut whole, &plan, &Seed::from_bytes([1; 32]), &Scope::default().all_addresses())
        .expect("anonymises");
    assert!(!text(&whole).contains("172.16.245.246"), "a fixture keeps no address of the network");
    let mut both = call();
    let scope = Scope::default().keeping(&cut).all_addresses();
    anonymize_in(&mut both, &plan, &Seed::from_bytes([1; 32]), &scope).expect("anonymises");
    assert!(!text(&both).contains("8.8.8.3"), "all_addresses wins over the kept set");
}
