//! The JSON-lines file CDR sink a runner composes from env (`B2BUA_CDR_FILE`):
//! [`FileCdrWriter`] appends one record per terminated call, encoded by its
//! [`CdrEncoder`] (default [`JsonRecordEncoder`]), each a line of the file.
//!
//! Contract: each record handed to `write` counts once, in `cdr_written_total`
//! when its line went to the file, else in `cdr_dropped_total`. A line goes
//! out in one `write_all` on a file opened for appending, so a reader of a
//! file several runs appended to reads whole lines. The sink writes inline:
//! `close` holds nothing.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use b2bua::cdr::{CdrEncoder, CdrRecord, CdrWriter, JsonRecordEncoder};
use b2bua::metrics::B2buaMetrics;
use call::Call;
use tokio::time::Instant;

use crate::env_read::stated;

/// The variable naming the file the sink appends to.
pub const CDR_FILE_VARIABLE: &str = "B2BUA_CDR_FILE";

/// A CDR sink appending each record to a file as one line.
pub struct FileCdrWriter {
    file: File,
    path: PathBuf,
    encoder: Arc<dyn CdrEncoder>,
    metrics: B2buaMetrics,
}

impl FileCdrWriter {
    /// The sink appending to `path`, created where absent, each record the
    /// bytes `encoder` produces; `Err` where the file cannot be opened.
    pub fn open(
        path: &Path,
        encoder: Arc<dyn CdrEncoder>,
        metrics: B2buaMetrics,
    ) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self { file, path: path.to_path_buf(), encoder, metrics })
    }

    /// The file it appends to.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// The file sink `B2BUA_CDR_FILE` selects as `lookup` states it, writing the
/// default [`JsonRecordEncoder`] record; `None` where it is unset or blank.
/// `Err` names the variable where the file cannot be opened, and both
/// variables where `B2BUA_CDR_RABBITMQ_URL` is set too: a worker has one sink.
pub fn file_cdr_writer_from_lookup(
    lookup: impl Fn(&str) -> Option<String>,
    metrics: &B2buaMetrics,
) -> Result<Option<FileCdrWriter>, String> {
    let Some(file) = stated(lookup(CDR_FILE_VARIABLE)) else { return Ok(None) };
    if stated(lookup("B2BUA_CDR_RABBITMQ_URL")).is_some() {
        return Err(format!(
            "{CDR_FILE_VARIABLE} and B2BUA_CDR_RABBITMQ_URL are both set: a worker writes its \
             CDRs to one sink, name one"
        ));
    }
    let path = PathBuf::from(file);
    let writer = FileCdrWriter::open(&path, Arc::new(JsonRecordEncoder), metrics.clone())
        .map_err(|e| format!("{CDR_FILE_VARIABLE}={}: {e}", path.display()))?;
    Ok(Some(writer))
}

#[async_trait]
impl CdrWriter for FileCdrWriter {
    async fn write(&self, _call: &Call, _terminated_at: i64) {}

    /// None: the records live in the file, not in process memory.
    async fn read_all(&self) -> Vec<CdrRecord> {
        Vec::new()
    }

    async fn close(&self, _deadline: Instant) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use b2bua::config::B2buaConfig;
    use b2bua::initial_invite::build_initial_call;
    use sip_message::parser::custom::CustomParser;
    use sip_message::{SipMessage, SipParser};
    use std::collections::HashMap;
    use std::net::SocketAddr;

    fn lookup(pairs: &[(&str, String)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> =
            pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
        move |k| map.get(k).cloned()
    }

    fn a_call(n: u64) -> Call {
        let raw = format!(
            "INVITE sip:bob@example.com SIP/2.0\r\n\
             Via: SIP/2.0/UDP 192.0.2.9:5060;branch=z9hG4bK-file{n}\r\n\
             Max-Forwards: 70\r\n\
             From: <sip:alice@example.com>;tag=alice{n}\r\n\
             To: <sip:bob@example.com>\r\n\
             Call-ID: file{n}@192.0.2.9\r\n\
             CSeq: 1 INVITE\r\n\
             Contact: <sip:alice@192.0.2.9:5060>\r\n\
             Content-Length: 0\r\n\r\n"
        );
        let req = match CustomParser::new().parse(raw.as_bytes()).expect("parse") {
            SipMessage::Request(r) => r,
            _ => panic!("expected a request"),
        };
        let cfg = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
        build_initial_call(
            &req,
            SocketAddr::from(([192, 0, 2, 9], 5060)),
            &cfg,
            &mut sip_ids::TurnIds::fixed(n),
            1_000,
        )
    }

    /// A directory of its own under the system's temporary one, emptied first.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cdr-file-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    /// Each record is appended after what the file held, one line of the
    /// encoder's bytes each, in the order written, and counted written.
    #[tokio::test]
    async fn the_file_sink_appends_each_record_as_one_line_after_what_the_file_held() {
        let file = scratch("appends").join("cdr.jsonl");
        std::fs::write(&file, "{\"earlier\":true}\n").expect("seed");
        let metrics = B2buaMetrics::new();
        let writer = file_cdr_writer_from_lookup(
            lookup(&[(CDR_FILE_VARIABLE, file.display().to_string())]),
            &metrics,
        )
        .expect("opens")
        .expect("selected");
        let calls = [a_call(1), a_call(2)];
        for call in &calls {
            writer.write(call, 2_000).await;
        }
        let mut expected = String::from("{\"earlier\":true}\n");
        for call in &calls {
            let line = JsonRecordEncoder.encode(call, 2_000).expect("encodes");
            expected.push_str(&String::from_utf8(line).expect("utf-8"));
            expected.push('\n');
        }
        assert_eq!(std::fs::read_to_string(&file).expect("read"), expected);
        assert_eq!((metrics.cdr_written_total(), metrics.cdr_dropped_total()), (2, 0));
    }

    /// Unset or blank, no file sink is selected.
    #[test]
    fn no_file_sink_is_selected_where_the_variable_is_unset_or_blank() {
        let metrics = B2buaMetrics::new();
        assert!(file_cdr_writer_from_lookup(lookup(&[]), &metrics).expect("ok").is_none());
        let blank = lookup(&[(CDR_FILE_VARIABLE, "  ".into())]);
        assert!(file_cdr_writer_from_lookup(blank, &metrics).expect("ok").is_none());
    }

    /// A file that cannot be opened refuses the boot naming the variable.
    #[test]
    fn a_file_that_cannot_be_opened_is_refused_naming_the_variable() {
        let file = scratch("unopenable").join("missing-dir").join("cdr.jsonl");
        let refused = file_cdr_writer_from_lookup(
            lookup(&[(CDR_FILE_VARIABLE, file.display().to_string())]),
            &B2buaMetrics::new(),
        )
        .err()
        .expect("refused");
        assert!(
            refused.starts_with(&format!("{CDR_FILE_VARIABLE}={}", file.display())),
            "{refused}"
        );
    }

    /// A file sink beside a broker URL is refused naming both variables.
    #[test]
    fn a_file_sink_beside_a_broker_url_is_refused_naming_both() {
        let file = scratch("both").join("cdr.jsonl");
        let refused = file_cdr_writer_from_lookup(
            lookup(&[
                (CDR_FILE_VARIABLE, file.display().to_string()),
                ("B2BUA_CDR_RABBITMQ_URL", "amqp://broker.example.com/%2f".into()),
            ]),
            &B2buaMetrics::new(),
        )
        .err()
        .expect("refused");
        assert!(
            refused.contains(CDR_FILE_VARIABLE) && refused.contains("B2BUA_CDR_RABBITMQ_URL"),
            "{refused}"
        );
    }
}
