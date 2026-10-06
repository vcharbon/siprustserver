//! The pipeline's recording fixture is pinned to the WRITER: the line under
//! `ts/pipeline/test/fixtures/recording/` is what `RecordedMessage::new` writes
//! for its own bytes, byte for byte, so the TypeScript confronter is fed a
//! line this crate stands behind.

use pivot_schema::bundle::RecordedMessage;

#[test]
fn the_pipeline_fixture_line_is_what_the_writer_writes() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../ts/pipeline/test/fixtures/recording/multipart-B.jsonl"
    );
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let line = text.lines().next().expect("one line");
    let read: RecordedMessage = serde_json::from_str(line).expect("the line decodes");
    let mut rebuilt = RecordedMessage::new(
        read.seq,
        read.dir,
        read.at_us,
        read.step.clone(),
        read.wire().to_vec(),
    );
    rebuilt.repeat_of = read.repeat_of;
    rebuilt.note = read.note.clone();
    assert_eq!(rebuilt, read, "the layout is derived from the bytes");
    assert_eq!(
        pivot_schema::canonical::format_line(&rebuilt).unwrap(),
        line,
        "the fixture is the writer's own line"
    );
}
