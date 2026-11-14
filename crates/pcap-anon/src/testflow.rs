//! Synthetic [`crate::model::Flows`] ladders for unit tests: every leg runs
//! over one socket pair whose `SUT` end is the platform and whose `PEER` end is
//! the simulated element, so an attempt's captured ladder is written as a list
//! of SUT requests and peer responses. Test-only support — no production path
//! builds a flow this way.

use sip_pcap::doc::Payload;

use crate::model::{ingest_str, Flows, Leg, Msg, Summary};
use crate::plan::Plan;
use crate::reading::Reading;

pub(crate) const SUT: &str = "10.0.0.1:5060";
pub(crate) const PEER: &str = "10.0.0.2:5060";

/// The invented numbering plan the unit tests run under. Country code 1
/// carries the plain numbers, all `+1 555` forms. The default country `28`
/// (national prefix `0`, 8-digit NSN, 5-digit short codes) and the 12-digit
/// country `899` are codes the ITU has not assigned (280–289 and 890–899 are
/// spare), so no number of theirs is a subscriber's. Two trunk families of
/// `1555` ids, private numbers of 4 to 7 digits.
pub(crate) const PLAN: &str = r#"{
  "default_cc": "28",
  "countries": [
    { "cc": "1", "nsn_len": 10 },
    { "cc": "28", "nsn_len": 8, "national_prefix": "0", "short_len": 5 },
    { "cc": "899", "nsn_len": 12 }
  ],
  "trunks": [
    { "prefix": "15550", "len": 7 },
    { "prefix": "15551234500", "len": 11 }
  ],
  "private_len": { "min": 4, "max": 7 },
  "max_composed_prefix": 6,
  "fake_prefix": "000"
}"#;

/// [`PLAN`], loaded.
pub(crate) fn plan() -> Plan {
    Plan::from_json_str(PLAN).expect("the test plan reads")
}

/// The reading the unit tests run under: RFC 3261 as written, no exemption,
/// and no bound below the datagram.
pub(crate) fn reading() -> Reading {
    Reading::stated(
        "max-header-length = 65535\nmax-uri-length = 65535\nmax-header-lines = 1000\n\
         max-datagram = 65535\n",
        "the test reading",
    )
    .expect("the test reading reads")
}

/// A request the peer sends to the SUT at `ms` (a caller's INVITE or CANCEL).
pub(crate) fn peer_req(ms: u64, method: &str) -> String {
    request(ms, method, PEER, SUT)
}

/// An INVITE-transaction response the SUT sends at `ms`.
pub(crate) fn sut_resp(ms: u64, status: u16) -> String {
    response(ms, status, SUT, PEER)
}

fn response(ms: u64, status: u16, src: &str, dst: &str) -> String {
    format!(
        r#"{{"ts_us":{},"src":"{src}","dst":"{dst}","hop":0,"retx":false,"raw":"x",
            "summary":{{"kind":"response","status":{status},"reason":"x",
            "cseq":{{"seq":1,"method":"INVITE"}},"from":{{"uri":"sip:a","tag":null}},
            "to":{{"uri":"sip:b","tag":null}}}}}}"#,
        ms * 1000
    )
}

fn request(ms: u64, method: &str, src: &str, dst: &str) -> String {
    format!(
        r#"{{"ts_us":{},"src":"{src}","dst":"{dst}","hop":0,"retx":false,"raw":"x",
            "summary":{{"kind":"request","method":"{method}","uri":"sip:x",
            "cseq":{{"seq":1,"method":"{method}"}},"from":{{"uri":"sip:a","tag":null}},
            "to":{{"uri":"sip:b","tag":null}}}}}}"#,
        ms * 1000
    )
}

/// One captured leg over the single `SUT`↔`PEER` hop.
pub(crate) fn leg(call_id: &str, msgs: &[String]) -> String {
    format!(
        r#"{{"call_id":"{call_id}","hops":[{{"a":"{SUT}","b":"{PEER}"}}],"invite":null,
            "final_status":null,"saw_180":false,"terminated_by":null,"tokens":[],"msgs":[{}]}}"#,
        msgs.join(",")
    )
}

/// One message from `src` to `dst` at `ts_us` on hop 0, its wire bytes `raw`.
pub(crate) fn message(ts_us: u64, src: &str, dst: &str, raw: &[u8], summary: Summary) -> Msg {
    Msg::new(ts_us, src.to_string(), dst.to_string(), 0, Payload::of_datagram(raw), summary)
}

/// One leg of Call-ID `call_id` carrying `msgs`, with no hop.
pub(crate) fn bare_leg(call_id: &str, msgs: Vec<Msg>) -> Leg {
    Leg {
        call_id: call_id.to_string(),
        hops: Vec::new(),
        invite: None,
        final_status: None,
        saw_180: false,
        terminated_by: None,
        opening_request_set_aside: false,
        tokens: Vec::new(),
        msgs,
    }
}

/// A document of `legs` and no group, its counters zero.
pub(crate) fn document(legs: Vec<Leg>) -> Flows {
    let mut flows = self::flows(&[], &[]);
    flows.legs = legs;
    flows
}

/// Ingest `legs` with the given correlation `groups` (leg indices).
pub(crate) fn flows(legs: &[String], groups: &[&[usize]]) -> Flows {
    let groups: Vec<String> = groups
        .iter()
        .map(|g| {
            let legs: Vec<String> = g.iter().map(|l| l.to_string()).collect();
            format!(r#"{{"legs":[{}],"evidence":[]}}"#, legs.join(","))
        })
        .collect();
    let json = format!(
        r#"{{"schema":5,
            "decode_stats":{{"records":0,"non_ip":0,"non_udp":0,"snap_truncated":0,
                             "datagrams":0,"fragments":0,"reassembled":0,"frag_dropped":0,"tail_truncated":0}},
            "flow_stats":{{"sip_messages":0,"capture_dups":0,"parse_failed":0,"non_sip":0}},
            "legs":[{}],"groups":[{}]}}"#,
        legs.join(","),
        groups.join(",")
    );
    ingest_str(&json).expect("synthetic flows ingest")
}
