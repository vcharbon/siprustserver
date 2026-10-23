//! A per-call index of an emitted flows document: the byte span of every leg
//! of `legs`, with its Call-ID, so one call's messages are read without
//! parsing a document of hundreds of megabytes. The index is a JSON file
//! beside the document ([`index_path`]) stating the size and modification
//! time of the document it was built from; an index that no longer matches
//! is stale, and the document is scanned again ([`scan`] reads it once,
//! structurally, without building its values).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::doc::{LegJson, MsgJson, Summary};

/// The index format; an index of another version is stale.
pub const INDEX_VERSION: u32 = 1;

/// One leg of the document: its Call-ID and the byte span of its object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegSpan {
    pub call_id: String,
    pub start: u64,
    pub end: u64,
}

/// The index of one document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallIndex {
    pub version: u32,
    /// The document's size and modification time (nanoseconds since the Unix
    /// epoch) when the index was built.
    pub size: u64,
    pub modified_ns: u128,
    pub legs: Vec<LegSpan>,
}

/// Where the index of `doc` lives: beside it, `<doc>.idx`.
pub fn index_path(doc: &Path) -> PathBuf {
    let mut name = doc.as_os_str().to_owned();
    name.push(".idx");
    PathBuf::from(name)
}

/// The size and modification time an index records of `doc`.
fn stamp(doc: &Path) -> Result<(u64, u128), String> {
    let meta = std::fs::metadata(doc).map_err(|e| format!("cannot stat {}: {e}", doc.display()))?;
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    Ok((meta.len(), modified))
}

/// Build the index of `doc` by one scan of its bytes.
pub fn build(doc: &Path) -> Result<CallIndex, String> {
    let (size, modified_ns) = stamp(doc)?;
    let bytes = std::fs::read(doc).map_err(|e| format!("cannot read {}: {e}", doc.display()))?;
    Ok(CallIndex { version: INDEX_VERSION, size, modified_ns, legs: scan(&bytes)? })
}

/// Build the index of `doc` and write it beside it.
pub fn write(doc: &Path) -> Result<CallIndex, String> {
    let index = build(doc)?;
    let path = index_path(doc);
    let json = serde_json::to_string(&index).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(index)
}

/// The index of `doc`: the one beside it when it still describes the
/// document, else a fresh scan, written back beside it where the directory
/// allows, so an index of another version or of an older document is
/// replaced the first time it is read. The flag says whether the stored one
/// served.
pub fn load(doc: &Path) -> Result<(CallIndex, bool), String> {
    let stored = std::fs::read_to_string(index_path(doc))
        .ok()
        .and_then(|text| serde_json::from_str::<CallIndex>(&text).ok());
    let (size, modified_ns) = stamp(doc)?;
    match stored {
        Some(index)
            if index.version == INDEX_VERSION
                && index.size == size
                && index.modified_ns == modified_ns =>
        {
            Ok((index, true))
        }
        _ => {
            let index = build(doc)?;
            if let Ok(json) = serde_json::to_string(&index) {
                let _ = std::fs::write(index_path(doc), json);
            }
            Ok((index, false))
        }
    }
}

/// The legs of `doc` whose Call-ID is `call_id`, read from their spans alone.
pub fn legs(doc: &Path, index: &CallIndex, call_id: &str) -> Result<Vec<LegJson>, String> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let mut file =
        std::fs::File::open(doc).map_err(|e| format!("cannot open {}: {e}", doc.display()))?;
    let mut out = Vec::new();
    for span in index.legs.iter().filter(|s| s.call_id == call_id) {
        let mut bytes = vec![0; (span.end - span.start) as usize];
        file.seek(SeekFrom::Start(span.start)).map_err(|e| e.to_string())?;
        file.read_exact(&mut bytes).map_err(|e| e.to_string())?;
        out.push(
            serde_json::from_slice(&bytes)
                .map_err(|e| format!("the index of {} no longer matches it: {e}", doc.display()))?,
        );
    }
    Ok(out)
}

/// Whether a message belongs to the transaction a selection names: CSeq
/// number `cseq` and CSeq method `method` (case-insensitive), each when given,
/// so a request and every response to it are selected together.
pub fn selects(msg: &MsgJson, cseq: Option<u32>, method: Option<&str>) -> bool {
    let (Summary::Request { cseq: c, .. } | Summary::Response { cseq: c, .. }) = &msg.summary;
    cseq.is_none_or(|n| c.seq == n) && method.is_none_or(|m| c.method.eq_ignore_ascii_case(m))
}

/// One container being read: an object or an array, the key it is under
/// when it is an object's member, whether it is the top-level `legs` array,
/// and, for a leg, where its object starts and the Call-ID read so far.
struct Frame {
    object: bool,
    expect_key: bool,
    key: Option<Vec<u8>>,
    legs_array: bool,
    leg: Option<(usize, Option<String>)>,
}

/// Every leg of a flows document's top-level `legs` array: its byte span and
/// its `call_id`. Reads the bytes once, tracking strings and nesting only.
pub fn scan(bytes: &[u8]) -> Result<Vec<LegSpan>, String> {
    let mut stack: Vec<Frame> = Vec::new();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                let end = string_end(bytes, i)?;
                let depth = stack.len();
                if let Some(top) = stack.last_mut() {
                    if top.object && top.expect_key {
                        top.key = Some(bytes[i + 1..end - 1].to_vec());
                        top.expect_key = false;
                    } else if top.key.as_deref() == Some(b"call_id".as_slice()) && depth == 3 {
                        if let Some((_, id)) = &mut top.leg {
                            *id = serde_json::from_slice(&bytes[i..end]).ok();
                        }
                    }
                }
                i = end;
                continue;
            }
            open @ (b'{' | b'[') => {
                let legs_array = open == b'['
                    && stack.len() == 1
                    && stack[0].key.as_deref() == Some(b"legs".as_slice());
                let in_legs = stack.last().is_some_and(|f| f.legs_array);
                stack.push(Frame {
                    object: open == b'{',
                    expect_key: open == b'{',
                    key: None,
                    legs_array,
                    leg: (in_legs && open == b'{').then_some((i, None)),
                });
            }
            b'}' | b']' => {
                let frame = stack.pop().ok_or("a flows document closes more than it opens")?;
                if let Some((start, call_id)) = frame.leg {
                    let call_id = call_id.ok_or("a leg of the flows document has no call_id")?;
                    spans.push(LegSpan { call_id, start: start as u64, end: (i + 1) as u64 });
                }
            }
            b',' => {
                if let Some(top) = stack.last_mut().filter(|f| f.object) {
                    top.expect_key = true;
                }
            }
            _ => {}
        }
        i += 1;
    }
    if !stack.is_empty() {
        return Err("the flows document ends inside a value".to_string());
    }
    Ok(spans)
}

/// The offset just past the string opening at `start`.
fn string_end(bytes: &[u8], start: usize) -> Result<usize, String> {
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Ok(i + 1),
            _ => i += 1,
        }
    }
    Err("the flows document ends inside a string".to_string())
}

#[cfg(test)]
mod tests;
