# Plan: Port the media layer to Rust (RTP negotiation, test-integrated)

## Context

The Rust server has ported the SIP **signaling** half but has no **media** layer yet
(`MIGRATION_STATUS.md` lists no media crate; only a minimal string-based SDP helper exists
in [sip-message/src/sdp.rs](../../crates/sip-message/src/sdp.rs) for REFER/fake-PRACK flows).
The TS project (`../sipjsserver/src/media/`) has a complete, test-integrated media subsystem
whose primary value is **verifying RTP/SDP negotiation**: an SDP offer/answer engine, RTP/RTCP
framing, G.711 codecs, a paced sender, and a spectral audio classifier that proves media
physically reached the right peer (so an SDP-rewrite bug shows up as `silence` instead of a
matched clip).

We will port it to Rust as a new `media` crate plus a `media-harness` test crate, templated
closely on the TS source, and integrate it into the existing test fabric the same way the SIP
layers are: bind endpoints on `SignalingNetwork`, run deterministically under a paused tokio
clock.

**Key architectural decision (this differs from "a fake impl + a real impl"):**
In this codebase, *test-vs-real is already solved one level down*, at the `SignalingNetwork`
seam — [`SimulatedSignalingNetwork`](../../crates/sip-net/src/simulated.rs) vs
[`RealSignalingNetwork`](../../crates/sip-net/src/real.rs). The media engine is written **once**,
transport-agnostic over `Arc<dyn SignalingNetwork>`, and runs unchanged on both (exactly like
`sip-txn`). The TS project's "two implementations" (`MediaEndpointTs` + `MediaEndpointRtpJs`)
are **not** test/real — they are two **RTP framing** implementations that cross-check each other
as independent conformance witnesses. We mirror that with a `RtpFraming` trait and two impls:
- `HandRolled` — RFC 3550 by hand (the `tsFraming` analog), primary under-test
- `WebRtcRs` — wraps the `rtp` / `rtcp` crates from webrtc-rs (the `rtp.js` analog), the witness

## Crate layout

Two new workspace members (add to `members` in [Cargo.toml](../../Cargo.toml)):

```
crates/media/            # prod: engine, framing, codec, sdp, rtcp
crates/media-harness/    # test-only: audio classifier + reference clips + negotiateCall helpers
```

Dependency edges (respecting ADR-0002 "one crate per layer", ADR-0004 "test-only harness"):

- `media` → `sip-net` (SignalingNetwork/UdpEndpoint/BindUdpOpts), `sip-clock` (Clock, timestamps
  only), `tokio`, `async-trait`, `thiserror`, and the witness libs `rtp` + `rtcp` (webrtc-rs).
- `media-harness` → `media`, `sip-net`, `layer-harness`, `rustfft` (FFT for the classifier),
  `tokio` (dev: `test-util`).
- `b2bua-harness` gains a `dev-dependency` on `media` + `media-harness` for slice 3b.

`Cargo.toml` deps follow the existing idiom in [sip-txn/Cargo.toml](../../crates/sip-txn/Cargo.toml):
workspace deps via `{ workspace = true }`, siblings via `{ path = "../x" }`. Add `rtp`, `rtcp`,
`rustfft`, `thiserror` to `[workspace.dependencies]`.

## `media` crate modules

Templated file-for-file on `../sipjsserver/src/media/`. All shared mutable transport state lives
behind `Arc<Mutex<…>>`; background loops (inbound recorder, RTCP reporter, paced sender) are
`tokio::spawn`ed tasks. **Behavioral timing uses `tokio::time::sleep` directly** — `Clock` is used
only for RTCP NTP timestamps (per CLAUDE.md: no separate fake-clock; behavior rides tokio time).

### `codec/g711.rs` — G.711 PCMA/PCMU
Port `../sipjsserver/src/media/codec/g711.ts` verbatim (canonical ITU segment tables). Surface:
```rust
pub enum G711Codec { Pcma, Pcmu }
pub fn alaw_encode(pcm: &[i16]) -> Vec<u8>;   pub fn alaw_decode(a: &[u8]) -> Vec<i16>;
pub fn mulaw_encode(pcm: &[i16]) -> Vec<u8>;  pub fn mulaw_decode(m: &[u8]) -> Vec<i16>;
pub fn g711_round_trip(pcm: &[i16], codec: G711Codec) -> Vec<i16>;
```

### `rtp/packet.rs` — framing trait + hand-rolled impl
Port `rtp/packet.ts`. `RtpHeader { version, padding, extension, marker, payload_type, sequence_number, timestamp, ssrc }`,
`RtpFramed { header, payload }`, and the seam:
```rust
pub trait RtpFraming: Send + Sync {
    fn name(&self) -> &'static str;                       // "ts" | "rtp.js" (kept as labels)
    fn encode_rtp(&self, h: &RtpHeader, payload: &[u8]) -> Vec<u8>;
    fn parse_rtp(&self, bytes: &[u8]) -> Option<RtpFramed>;
}
pub struct HandRolled;   // 12-byte header encode; full parse honoring CSRC/ext/padding
```
`HandRolled::parse_rtp` mirrors the TS full parser (version==2 check, CSRC skip, extension words,
padding trim), so it reads any peer's packets.

### `rtp/webrtc_framing.rs` — webrtc-rs witness
`pub struct WebRtcRs;` implementing `RtpFraming` via `rtp::packet::Packet` /
`rtp::header::Header` (encode via `Marshal`, parse via `Unmarshal`). Direct analog of
`native/MediaEndpointRtpJs.ts`'s `rtpJsFraming`.

### `rtp/rtcp.rs` — SR/RR (counts-only)
Port `rtp/rtcp.ts`: `encode_sender_report(SenderReportFields)`, `encode_receiver_report(ssrc)`,
`is_rtcp(bytes)` (PT in 200..=204, RFC 5761 mux demux), NTP-from-ms helper
(`NTP_EPOCH_OFFSET = 2_208_988_800`). Hand-rolled bytes (the webrtc `rtcp` crate is available if we
later want a witness, but counts-only SR/RR is trivial enough to hand-roll like TS).

### `sdp/mod.rs` — structured SDP types, parse, build
The existing [sip-message/src/sdp.rs](../../crates/sip-message/src/sdp.rs) is string-based and
scoped to b2bua hold/transfer; **leave it untouched**. Add a structured SDP model here, porting
`media/sdp/types.ts` + the parse/build used by the negotiator:
```rust
pub enum MediaDirection { SendRecv, SendOnly, RecvOnly, Inactive }
pub struct SdpRtpMap { payload_type: u8, encoding_name: String, clock_rate: u32 }
pub struct SdpMedia { kind: String, port: u16, protocol: String, formats: Vec<u8>,
                      rtpmap: Vec<SdpRtpMap>, direction: MediaDirection, connection_addr: Option<String> }
pub struct Sdp { origin: SdpOrigin, connection_addr: Option<String>, media: Vec<SdpMedia> }
pub fn parse_sdp(text: &str) -> Result<Sdp, SdpParseError>;
pub fn build_sdp(/* origin, media */) -> Sdp;  pub fn encode_sdp(&Sdp) -> String;
```

### `sdp/negotiator.rs` — RFC 3264/3262/5009 offer/answer engine
Port `media/sdp/negotiator.ts` exactly, including the typed-error taxonomy and the state machine.
```rust
pub enum NegotiationState { Idle, OfferSent, Early, Committed, Held }
pub struct NegotiatedMedia { remote: NetAddr, codec: CodecDesc, direction: MediaDirection,
                             send: bool, receive: bool }
pub enum SdpRule { AnswerCodecNotInOffer, EmptyCodecIntersection, MLineCountMismatch,
                   AnswerWithoutOffer, GlareSecondOffer, NoMedia }  // each carries rfc + message
pub struct SdpNegotiationError { rule: SdpRule, rfc: &'static str, message: String }

pub struct OfferAnswerEngine { /* local_addr, codecs, state, pending_offer, negotiated, session_version */ }
impl OfferAnswerEngine {
    pub fn local_offer(&mut self) -> Sdp;                                          // → OfferSent
    pub fn answer_to(&mut self, offer: &Sdp) -> Result<Sdp, SdpNegotiationError>;  // UAS
    pub fn apply_remote(&mut self, sdp: &Sdp, reliable: bool) -> Result<NegotiatedMedia, SdpNegotiationError>; // UAC
    pub fn negotiated(&self) -> Option<&NegotiatedMedia>;
    pub fn state(&self) -> NegotiationState;
}
pub fn is_early_media_authorized(has_sdp: bool, p_early_media: Option<MediaDirection>) -> bool;
```
Reproduce the exact guards: glare (`OfferSent` + inbound offer → `GlareSecondOffer`), `no-media`,
first-supported-codec-honoring-offerer-order, `reverse_direction`, reachability (port≠0 and
addr≠0.0.0.0) → `send`, `applyRemote` codec-must-be-in-offer + m-line-count match + reliable→commit
vs provisional→`Early`.

### `transport.rs` — the engine (`mediaEndpointLayer` analog)
Port `media/transport.ts`. Constructor takes the framing impl and the network:
```rust
pub struct MediaEndpoint { net: Arc<dyn SignalingNetwork>, framing: Arc<dyn RtpFraming>, /* port alloc */ }
impl MediaEndpoint {
    pub fn new(net: Arc<dyn SignalingNetwork>, framing: Arc<dyn RtpFraming>) -> Self;
    pub async fn open(&self, local_ip: &str, local_port: Option<u16>, opts: OpenOptions)
        -> Result<MediaTransport, BindError>;
}
```
`MediaTransport` holds the bound `UdpEndpoint` + `Arc<Mutex<TransportInner>>` and spawns:
- **inbound recorder loop**: `loop { ep.recv().await }` → `handle_packet` (RTCP? count; else
  `framing.parse_rtp` → demux into `sources: HashMap<(ip,port,ssrc), Bucket>`, decode payload,
  append PCM continuously regardless of active peer).
- **RTCP reporter**: `interval(rtcp_interval_ms)` → SR if `out_packets>0` else RR, send to `last_remote`.

`session(dialog_id)`, `sources()`, `active_peer()`, `stats()`, `send_raw()` mirror the TS
`MediaTransport`. `MediaSession` mirrors TS: `configure(NegotiatedMedia)`, `commit(reason)`
(sets active, marks siblings abandoned, sets `last_remote`), `play(PlayScript)` (spawns paced
sender: per-frame 160 samples @ 20 ms, guard `active && !abandoned && neg.send`, bump seq/ts,
encode, `send_to`, `sleep(ptime)`), `recorded()` (merge PCM from buckets matching negotiated
remote), `is_active()`. Constants: `SAMPLE_RATE=8000`, `DEFAULT_PTIME_MS=20`,
`DEFAULT_RTCP_INTERVAL_MS=5000`, `AUTO_PORT_BASE=40000` (step 2).

### `lib.rs` — re-exports + convenience constructors
`pub fn ts_endpoint(net) -> MediaEndpoint` (HandRolled) and
`pub fn webrtc_endpoint(net) -> MediaEndpoint` (WebRtcRs) — the `MediaEndpointTs` /
`MediaEndpointRtpJs` analogs. `CodecDesc`, `PCMA`, `PCMU`, `NetAddr`, `PlayScript`,
`MediaStreamStats`, `SourceBucket`, `OpenOptions`, `MediaNegotiationError` re-exported.

## `media-harness` crate (test-only) — slice 0 + helpers

### `audio/clips.rs` — deterministic reference clips
Port `../sipjsserver/src/test-harness/media/audio/clips.ts`: formant-synthesis voices
(`alice`, `bob`, `charlie`) + 425 Hz `ringback` with 1 s on/off cadence; 8 kHz, 2 s, `Vec<i16>`,
no RNG. `reference_clip(name) -> Vec<i16>`, `reference_clips() -> BTreeMap<ClipName, Vec<i16>>`.

### `audio/classify.rs` — the verdict engine (faithful MFCC port)
Port `classify.ts`: `clip_signature` = MFCC over framed FFT (`rustfft`), then cosine distance to
each reference; margin gate + RMS silence floor.
```rust
pub enum Classification { Matched, Ambiguous, Silence, NoAudio }
pub struct MediaVerdict { classification, matched: Option<ClipName>, distance, margin,
                          distances: BTreeMap<ClipName,f64>, rms }
pub fn classify(pcm: &[i16], opts: ClassifyOptions) -> MediaVerdict;  // defaults: margin .05, rms_floor .01, min 800 samples
pub fn classify_sequence(pcm: &[i16], opts) -> Vec<Segment>;          // for ringback→voice ordering
```

### `negotiate.rs` — slice 3a helpers
Port `tests/media/support-negotiate.ts`: `negotiate_call(alice, bob, opts)` runs the full
offer→answer→applyRemote→commit exchange with pluggable `rewrite_offer` / `rewrite_answer`
closures, returning the two committed `MediaSession`s. Plus `corrupt_connection_addr(ip)` rewrite.

## Test slices (mirror `../sipjsserver/docs/media-testing.md`)

All deterministic tests use `#[tokio::test(start_paused = true)]` and advance via
`sip_clock::testkit::advance_in_100ms_chunks` (or a local `advance`). The simulated fabric is
`Arc::new(SimulatedSignalingNetwork::new(1))` (transit delay 1 ms — **never 0**, per CLAUDE.md).
Live tests use `RealSignalingNetwork` + real `tokio::time::sleep` with a small packet-loss
tolerance.

- **Slice 0** `media-harness/tests/audio_comparator.rs`: clips self-classify with margin;
  cross-separation; empty→`NoAudio`; dithered→`Silence`; G.711 round-trip still matches;
  ringback→voice sequence ordering.
- **Slice 1** `media/tests/rtp_media.rs`: the 2×2 framing cross-check matrix
  (`HandRolled↔HandRolled`, `HandRolled↔WebRtcRs`, `WebRtcRs↔HandRolled`, `WebRtcRs↔WebRtcRs`,
  the `expectHeaderRoundTrip` analog); play→record over the simulated fabric for **both** endpoint
  flavors (`classify(recorded)` is `Matched`+correct name; exact packet/byte stats:
  `frames * (12 + 160)`); RTCP SR/RR counts flow on a shortened interval.
- **Slice 1-live** `media/tests/rtp_media_live.rs`: same play→record over `RealSignalingNetwork`,
  `>= ceil(len/160) - 2` packet tolerance.
- **Slice 2** `media/tests/sdp_negotiation.rs`: offerer order honored; each negative path asserts
  the exact `SdpRule`; port 0 / `a=inactive` kill send (and `state()==Held`);
  `is_early_media_authorized` truth table.
- **Slice 3a** `media/tests/media_e2e.rs`: `negotiate_call` transparent relay → both hear each
  other; `corrupt_connection_addr` → victim hears `Silence`/`NoAudio` and has 0 sources, while the
  misdirected sender's media physically lands at the wrong port (other side's `sources() > 0`).
- **Slice 3b** `b2bua-harness/tests/basic_call_media.rs`: drive a real call through `B2buaSut`
  using the existing imperative agent API (template: [refer_allow.rs](../../crates/b2bua-harness/tests/refer_allow.rs)).
  Each `Agent` gets a `MediaTransport` bound at its advertised RTP address; the INVITE carries a
  real SDP offer built by the engine, the 200 carries the engine's answer; after ACK both
  `play(...)`, advance ~3 s, then assert `classify(recorded)` matches the peer. Because the Rust
  B2BUA is **signaling-only** (no RTP relay), media flows peer-to-peer to the addresses in the SDP
  the B2BUA passed/rewrote — so this test catches B2BUA SDP-rewrite regressions exactly as TS
  slice 3b does. (HTML media-panel reporting is out of scope; the assertion is the `MediaVerdict`.)

## Files to create / modify

- **Create**: `crates/media/` (Cargo.toml, src/{lib,transport,codec/g711,rtp/{packet,webrtc_framing,rtcp},sdp/{mod,negotiator}}.rs, tests/*),
  `crates/media-harness/` (Cargo.toml, src/{lib,audio/{clips,classify},negotiate}.rs, tests/audio_comparator.rs),
  `crates/b2bua-harness/tests/basic_call_media.rs`.
- **Modify**: [Cargo.toml](../../Cargo.toml) (add 2 members + `rtp`/`rtcp`/`rustfft`/`thiserror`
  to workspace deps), [crates/b2bua-harness/Cargo.toml](../../crates/b2bua-harness/Cargo.toml)
  (dev-deps), [MIGRATION_STATUS.md](../../MIGRATION_STATUS.md) (add the media row with the source
  release pinned per CLAUDE.md migration protocol), and the source-trace hint note if useful.
- **Do not touch**: [sip-message/src/sdp.rs](../../crates/sip-message/src/sdp.rs) (b2bua-scoped).

## Implementation order

1. Scaffold `media` crate + workspace wiring; `codec/g711.rs` + unit round-trip test (no network).
2. `rtp/packet.rs` (HandRolled) + `rtp/webrtc_framing.rs` (WebRtcRs) + slice-1 cross-check matrix.
3. `rtp/rtcp.rs`.
4. `sdp/{mod,negotiator}.rs` + slice-2 conformance tests (pure, no network).
5. `transport.rs` (bind, inbound demux, paced sender, RTCP loop, sessions/active-peer) + slice-1
   play→record over the simulated fabric; then slice-1-live.
6. `media-harness`: `audio/clips.rs` + `audio/classify.rs` + slice-0 tests; then `negotiate.rs`
   + slice-3a `media_e2e.rs`.
7. Slice-3b `b2bua-harness/tests/basic_call_media.rs`.
8. `MIGRATION_STATUS.md` update + list any TS tests intentionally not ported with justification
   (per CLAUDE.md migration protocol).

## Hazards to respect (from CLAUDE.md)

- Simulated transit delay must be **≥ 1 ms** (already enforced in `SimulatedSignalingNetwork::new`).
- **Drive the protocol between advances**: advance to the send window, let packets land, advance
  again — don't step past two deadlines at once (e.g. RTCP interval *and* call teardown).
- No separate fake-clock: behavioral timing is `tokio::time`; `Clock` is timestamps only.
- A panicking scenario loses its trace; debug media flow with temporary `eprintln!` in the inbound
  loop / sender if a slice misbehaves.

## Verification

- `source ~/.cargo/env` first (cargo not on PATH — see memory).
- `cargo test -p media -p media-harness` — slices 0/1/2/3a green; framing cross-check proves the
  hand-rolled and webrtc-rs wire formats agree.
- `cargo test -p media --test rtp_media_live` — real-UDP wire proof.
- `cargo test -p b2bua-harness --test basic_call_media` — slice 3b: both peers hear each other
  through the real B2BUA; flipping in a deliberate SDP corruption turns the verdict to `Silence`,
  proving the test actually detects misnegotiation.
- `cargo clippy --all-targets` clean for the new crates.
