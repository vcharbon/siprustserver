//! The TypeScript recording fixtures are pinned to the WRITER: each line under
//! `ts/pipeline/test/fixtures/recording/` and `ts/rig/test/fixtures/user-identities/`
//! is what `RecordedMessage::new` writes for its own bytes, byte for byte, so
//! the confronter and the cell description are fed lines this crate stands
//! behind, the user identities `sip-message` read included.

use pivot_schema::bundle::RecordedMessage;

/// Every line of the fixture at `relative` (from the repository root), each
/// asserted to be the writer's own line for its bytes.
fn writer_lines(relative: &str) -> Vec<RecordedMessage> {
    let path = format!("{}/../../{relative}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    text.lines()
        .map(|line| {
            let read: RecordedMessage = serde_json::from_str(line).expect("the line decodes");
            let mut rebuilt = RecordedMessage::new(
                read.seq,
                read.dir,
                read.at_us,
                read.step.clone(),
                read.wire().to_vec(),
                &sip_message::CustomParser::new(),
            );
            rebuilt.repeat_of = read.repeat_of;
            rebuilt.note = read.note.clone();
            assert_eq!(rebuilt, read, "the layout and identities are derived from the bytes");
            assert_eq!(
                pivot_schema::canonical::format_line(&rebuilt).unwrap(),
                line,
                "the fixture is the writer's own line"
            );
            read
        })
        .collect()
}

#[test]
fn the_pipeline_fixture_line_is_what_the_writer_writes() {
    assert_eq!(writer_lines("ts/pipeline/test/fixtures/recording/multipart-B.jsonl").len(), 1);
}

/// The INVITE's From carries a user-parameter, its To an RFC 3966 visual
/// separator under `user=phone`, its Request-URI is a tel URI: each line states
/// the subscriber `Uri::user_identity` reads.
#[test]
fn the_identity_fixture_lines_state_the_users_sip_message_reads() {
    let lines = writer_lines("ts/rig/test/fixtures/user-identities/recording/A.jsonl");
    assert_eq!(lines.len(), 5);
    let invite = lines[0].identities.as_ref().expect("the INVITE parses");
    let users = [
        invite.from.user.as_deref(),
        invite.to.user.as_deref(),
        invite.ruri.as_ref().and_then(|ruri| ruri.user.as_deref()),
    ];
    assert_eq!(users, [Some("+15550100"), Some("+15550101"), Some("+15550102")]);
    let answer = lines[1].identities.as_ref().expect("the 200 parses");
    assert_eq!(answer.ruri, None, "a response names no Request-URI");
}
