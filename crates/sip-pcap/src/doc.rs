//! The emitted flows document: the wire contract downstream tooling reads.
//!
//! Types only — building them from the flow model is [`crate::emit`], deriving
//! their enrichment is [`crate::enrich`]. Serde and schemars derive from the
//! same declarations, so the exported JSON Schema cannot drift from the emit.
//!
//! Two document-wide conventions:
//!
//! - **Omission.** An optional value is omitted when absent, a collection when
//!   empty. `null` appears only where the schema-4 fields already used it
//!   (`invite`, `final_status`, `terminated_by`, `from.tag`, `to.tag`).
//! - **Enrichment is derived.** Every field beyond the schema-4 core is a pure
//!   function of the message bytes and of the document's own structure, so a
//!   transformed document (anonymized, filtered) re-derives them and can never
//!   carry an enrichment its bytes no longer justify.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Value of the top-level `schema` field. Bumped on any breaking change to the
/// emitted shape; consumers reject versions they do not know.
pub const EMIT_SCHEMA_VERSION: u32 = 5;

/// A whole capture as `sipflow --json` emits it.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FlowsDoc {
    pub schema: u32,
    /// The header allow-list `msgs[].headers` projects, canonical names in the
    /// order requested. Empty ⇒ no message carries a `headers` projection.
    #[serde(default)]
    pub emit_headers: Vec<String>,
    pub decode_stats: DecodeStatsJson,
    pub flow_stats: FlowStatsJson,
    /// Index = the leg id `groups[].legs` and every evidence entry reference.
    pub legs: Vec<LegJson>,
    /// Ordered by first activity; every leg is in exactly one group.
    pub groups: Vec<GroupJson>,
}

/// pcap-decode counters — they describe the WHOLE capture even when the
/// document is restricted to selected groups.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DecodeStatsJson {
    pub records: u64,
    pub non_ip: u64,
    pub non_udp: u64,
    pub snap_truncated: u64,
    pub datagrams: u64,
    pub fragments: u64,
    pub reassembled: u64,
    pub frag_dropped: u64,
    pub tail_truncated: u64,
}

/// SIP-classification counters over the decoded datagrams.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FlowStatsJson {
    pub sip_messages: u64,
    pub capture_dups: u64,
    pub parse_failed: u64,
    pub non_sip: u64,
}

/// All messages sharing one Call-ID, split by observation hop.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LegJson {
    pub call_id: String,
    /// Observation vantages ordered by first observation; `msgs[].hop` indexes
    /// here, so filtering on it yields the leg as seen at ONE capture point.
    pub hops: Vec<HopJson>,
    pub invite: Option<InviteJson>,
    /// First final (>=200) response to the initial INVITE.
    pub final_status: Option<u16>,
    pub saw_180: bool,
    /// `"BYE"` | `"CANCEL"` — the first teardown request seen.
    pub terminated_by: Option<String>,
    /// Union over all token strategies, sorted and deduped; which strategy a
    /// value came from is in the group evidence.
    pub tokens: Vec<String>,
    /// Capture-time order across all hops.
    pub msgs: Vec<MsgJson>,
}

/// An observed socket pair, `"ip:port"` (IPv6 bracketed), direction-insensitive.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HopJson {
    pub a: String,
    pub b: String,
}

/// The leg's initial INVITE, as text.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct InviteJson {
    pub ruri: String,
    pub from_uri: String,
    pub to_uri: String,
    pub cseq: u32,
}

/// One captured SIP message: the exact wire bytes, the compact summary, and
/// the derived enrichment.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MsgJson {
    /// Capture timestamp, microseconds since the Unix epoch.
    pub ts_us: u64,
    pub src: String,
    pub dst: String,
    /// Index into the owning leg's `hops`.
    pub hop: usize,
    /// The same-transaction half of `repeat_of`: the relation holds AND the top
    /// Via branch matches — a SIP retransmission. Capture-stack duplicates are
    /// collapsed before the model and never appear.
    pub retx: bool,
    /// WHICH PROBE WROTE THIS COPY — one id per classic-pcap file, one per
    /// pcapng interface per section, numbered across the whole read. A
    /// `mergecap` of several probes writes one packet once per probe, and this
    /// is the only field that differs between the copies. Absent in a schema-4
    /// document and in one whose capture declared a single observation point.
    #[serde(default, skip_serializing_if = "is_first_probe")]
    pub probe: u32,
    /// Index into this leg's `msgs` of the EARLIEST message this one repeats,
    /// under the criterion in [`crate::callfacts::mark_repeats`]: `retx` is the
    /// same-branch half, and a fresh-branch re-answer (a peer ACKing a
    /// retransmitted final in a new transaction) is the half `retx` cannot see.
    /// Both are bounded by the transaction envelope, so matching bytes emitted
    /// after it carry NEITHER — they are a fresh event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat_of: Option<usize>,
    #[serde(flatten)]
    pub payload: Payload,
    pub summary: Summary,
    // Enrichment — every field below is derived (see the module doc) and
    // defaults to "absent" so a schema-4 document can be read and re-derived.
    /// Via chain, top first — one entry per hop the message names.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub via: Vec<ViaJson>,
    /// The `emit_headers` allow-list as this message carries it: wire order,
    /// duplicates kept.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<HeaderJson>,
    #[serde(default)]
    pub identities: Identities,
    /// RFC 3262 `RSeq` — the reliable-provisional identity within its INVITE
    /// transaction, derived so `repeat_of` can tell two same-status 18x apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rseq: Option<String>,
    /// RFC 3891 `Replaces`, pre-parsed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces: Option<DialogRef>,
    /// RFC 3515 `Refer-To`, pre-parsed, escaped `?Replaces=` resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refer_to: Option<ReferToJson>,
    /// Present iff the message carries a body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<BodyJson>,
}

/// Exact wire bytes, in EXACTLY ONE of three forms chosen purely from the
/// bytes so re-emitting a transformed model is deterministic. Reassembly:
/// `raw` as UTF-8 | `head` as UTF-8 ++ decode(`body_b64`) | decode(`raw_b64`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Payload {
    /// Whole payload is valid UTF-8 — the common, diff-readable case.
    Text { raw: String },
    /// Start line + headers + blank line as UTF-8, then a binary body as
    /// standard base64. A binary MIME part is what produces this form.
    HeadBody { head: String, body_b64: String },
    /// Even the head is not UTF-8 — opaque, standard base64.
    Opaque { raw_b64: String },
}

impl Payload {
    /// The form these bytes take, chosen purely from the bytes: whole-UTF-8 ⇒
    /// [`Payload::Text`]; else a UTF-8 head whose tail is exactly `body` ⇒
    /// [`Payload::HeadBody`]; else [`Payload::Opaque`]. Deterministic, so a
    /// document re-emitted after a transformation keeps the same encoding.
    pub fn of(raw: &[u8], body: &[u8]) -> Self {
        if let Ok(s) = std::str::from_utf8(raw) {
            return Payload::Text { raw: s.to_string() };
        }
        let head_len = raw.len().saturating_sub(body.len());
        if !body.is_empty() && raw[head_len..] == *body {
            if let Ok(head) = std::str::from_utf8(&raw[..head_len]) {
                return Payload::HeadBody { head: head.to_string(), body_b64: base64(body) };
            }
        }
        Payload::Opaque { raw_b64: base64(raw) }
    }

    /// The exact wire bytes, whichever form carries them.
    pub fn bytes(&self) -> Result<Vec<u8>, String> {
        match self {
            Payload::Text { raw } => Ok(raw.clone().into_bytes()),
            Payload::HeadBody { head, body_b64 } => {
                let mut out = head.clone().into_bytes();
                out.extend(unbase64(body_b64)?);
                Ok(out)
            }
            Payload::Opaque { raw_b64 } => unbase64(raw_b64),
        }
    }

    /// The BODY bytes alone, or `None` where this form does not carry them.
    /// A whole-UTF-8 payload states them after the blank line that ends the
    /// head, a split payload states them base64, and an opaque one states
    /// nothing readable. A terminated head with nothing after it carries an
    /// EMPTY body — a fact, not an absence.
    pub fn body(&self) -> Option<Vec<u8>> {
        match self {
            Payload::Text { raw } => sip_message::sniff::body(raw.as_bytes()).map(<[u8]>::to_vec),
            Payload::HeadBody { body_b64, .. } => unbase64(body_b64).ok(),
            Payload::Opaque { .. } => None,
        }
    }
}

fn base64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn unbase64(text: &str) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(text).map_err(|e| e.to_string())
}

/// Probe 0 is the only observation point most captures declare, so writing it
/// on every message would double the size of every document for nothing.
fn is_first_probe(probe: &u32) -> bool {
    *probe == 0
}

impl MsgJson {
    /// A message carrying its capture facts and no enrichment yet — the shape
    /// [`crate::enrich`] fills in.
    pub fn new(
        ts_us: u64,
        src: String,
        dst: String,
        hop: usize,
        payload: Payload,
        summary: Summary,
    ) -> Self {
        Self {
            ts_us,
            src,
            dst,
            hop,
            retx: false,
            probe: 0,
            repeat_of: None,
            payload,
            summary,
            via: Vec::new(),
            headers: Vec::new(),
            identities: Identities::default(),
            rseq: None,
            replaces: None,
            refer_to: None,
            body: None,
        }
    }
}

/// The compact parsed projection of a message.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Summary {
    Request { method: String, uri: String, cseq: CSeqJson, from: Party, to: Party },
    Response { status: u16, reason: String, cseq: CSeqJson, from: Party, to: Party },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CSeqJson {
    pub seq: u32,
    pub method: String,
}

/// From/To identity as parsed: URI text plus the dialog tag.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Party {
    pub uri: String,
    pub tag: Option<String>,
}

/// One Via hop.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ViaJson {
    /// `host` or `host:port` exactly as the hop declared it.
    pub sent_by: String,
    /// `UDP` / `TCP` / `TLS` …
    pub transport: String,
    pub branch: Option<String>,
    /// The `;received=` the next hop stamped, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub received: Option<String>,
}

/// One allow-listed header instance.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HeaderJson {
    /// Canonical name — compact forms and casing already resolved.
    pub name: String,
    /// The message's own spelling, present only when it differs from `name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire: Option<String>,
    /// The value with continuation lines unfolded; comma-folded lines are NOT
    /// split, so what the wire put on one line stays one entry.
    pub value: String,
}

/// The user identities a message names.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct Identities {
    pub from: Identity,
    pub to: Identity,
    /// Request-URI — requests only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ruri: Option<Identity>,
    /// Every P-Asserted-Identity, in wire order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pai: Vec<Identity>,
}

/// One URI reduced to the subscriber it names.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct Identity {
    /// The URI as the wire spelled it.
    pub uri: String,
    /// `sip-message`'s canonical user identity: user-parameters (`;npdi`,
    /// `;verstat`) dropped, RFC 3966 visual separators removed on a
    /// phone-shaped user. Absent for a userless URI.
    pub user: Option<String>,
    /// `user` reduced to digits with one leading `00`/`0` dropped — the form a
    /// number comparison uses. Absent when the identity carries no digit.
    pub digits: Option<String>,
}

/// A dialog named by `Replaces`, here or inside a `Refer-To`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DialogRef {
    pub call_id: String,
    pub to_tag: Option<String>,
    pub from_tag: Option<String>,
}

/// A `Refer-To` target.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReferToJson {
    pub target: Identity,
    /// The escaped `?Replaces=` an attended transfer carries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces: Option<DialogRef>,
}

/// The message body's layout. A multipart body arrives ALREADY SPLIT: no
/// consumer downstream owns MIME.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BodyJson {
    /// Media type, parameters dropped. Empty when the message declares none.
    pub content_type: String,
    /// Body length in bytes.
    pub len: usize,
    /// MIME parts in body order; empty for a single-part body.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<PartJson>,
}

/// One MIME part, located rather than copied: `offset`/`len` index the body
/// bytes the message already carries, so the document holds each byte once and
/// a transformed body cannot disagree with its parts.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PartJson {
    /// The part's own `Content-Type`, parameters included; `text/plain` when
    /// the part declares none (RFC 2045 §5.2).
    pub content_type: String,
    /// The part's `Content-ID`, angle brackets as written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_id: Option<String>,
    /// The part's remaining entity headers in wire order, name and value as
    /// written (RFC 2045 §3) — `Content-Transfer-Encoding`,
    /// `Content-Disposition`, and any other the part states. `Content-Type` and
    /// `Content-ID` have their own fields and are not repeated here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<PartHeaderJson>,
    /// Offset of the part's CONTENT (after its blank line) into the body.
    pub offset: usize,
    pub len: usize,
}

/// One entity header of a MIME part, as the part wrote it. Unlike a message
/// header it is not canonicalized: a part replays under the spelling it came in
/// with.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PartHeaderJson {
    pub name: String,
    pub value: String,
}

/// Correlated legs of one call, plus the per-call facts every consumer would
/// otherwise recompute by scanning every message of every leg.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GroupJson {
    pub legs: Vec<usize>,
    /// Why members were joined (empty for a single-leg group) — heuristic,
    /// meant for human confirm/override downstream.
    pub evidence: Vec<Evidence>,
    // Enrichment — derived, and defaulted so a schema-4 document reads.
    /// First capture timestamp across the group's legs.
    #[serde(default)]
    pub t0_us: u64,
    /// The call's OWN initial INVITE, as opposed to a b-leg INVITE.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_invite: Option<MsgRef>,
    /// Timestamp of the terminal INVITE response (see `final_status`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_us: Option<u64>,
    /// The call's terminal status: the status of the LAST response of status
    /// 200 or above to an INVITE, in capture-time order across every leg of
    /// the group. Absent when the group carries none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_status: Option<u16>,
    /// Request methods the call carries, with the body media types each one
    /// sent — enough to classify a call by what it does (an INFO exchange with
    /// XML bodies, an INVITE with SDP) without reading a message.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub methods: BTreeMap<String, MethodFacts>,
}

/// A message coordinate inside the document.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
pub struct MsgRef {
    pub leg: usize,
    pub msg: usize,
}

/// What one request method did across a call.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MethodFacts {
    /// Requests of this method, retransmissions and repeats included.
    pub requests: u32,
    /// Distinct body media types those requests carried, sorted. A multipart
    /// body contributes its own type AND every part's type.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content_types: Vec<String>,
}

/// Why legs were grouped, and which pipeline strategy fired (`strategy` indexes
/// the correlation pipeline; grouping is first-wins, evidence is not).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Evidence {
    SharedToken {
        strategy: usize,
        token: String,
        legs: Vec<usize>,
    },
    SharedHeaderParam {
        strategy: usize,
        header: String,
        param: String,
        token: String,
        legs: Vec<usize>,
    },
    /// `legs[1]`'s Call-ID is `prefix` ++ `legs[0]`'s: an application server
    /// re-originated the call from `as_socket` back to `peer_socket`.
    DerivedCallId {
        strategy: usize,
        legs: Vec<usize>,
        prefix: String,
        as_socket: String,
        peer_socket: String,
        shared_hop: bool,
        dt_us: u64,
    },
    IdentityAdjacency {
        strategy: usize,
        legs: Vec<usize>,
        shared_host: String,
        dt_us: u64,
    },
}
