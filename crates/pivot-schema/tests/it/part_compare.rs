//! A MIME part of an expected multipart body states how its received
//! counterpart is compared, with the same `compare` a single resource body
//! carries (`PCAP2TEST_PIVOT_V3.md` §8.3).

use pivot_schema::body::{BodyCompare, Part};

#[test]
fn a_part_states_its_compare_mode_as_a_resource_body_does() {
    let text = r#"{"content-type":"application/sdp","ref":"resources/uas1_r0_0.sdp","rewrite":["c=addr","m=port"],"compare":"sdp"}"#;
    let part: Part = serde_json::from_str(text).unwrap();
    assert_eq!(part.compare, Some(BodyCompare::Sdp));
    let xml: Part = serde_json::from_str(
        r#"{"content-type":"application/pidf+xml","ref":"resources/uas1_r0_2.xml","mode":"frozen","compare":"xml"}"#,
    )
    .unwrap();
    assert_eq!(xml.compare, Some(BodyCompare::Xml));
    let exact: Part = serde_json::from_str(
        r#"{"content-type":"application/vnd.example.blob","ref":"resources/uas1_r0_1.bin","mode":"frozen"}"#,
    )
    .unwrap();
    assert_eq!(exact.compare, None, "absent means byte for byte");
}
