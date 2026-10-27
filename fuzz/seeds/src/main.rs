//! Writes each fuzz target's seed corpus, one directory per target under
//! `<out>`, each file one input in the target's layout (`fuzz_lanes::input`):
//!
//!   fuzz-seeds <out> [--target <t>]     the no-panic lanes' synthetic seeds, of
//!                                       every target or of <t>
//!   fuzz-seeds <out> --messages <path>  raw SIP messages, into the targets that
//!                                       take one: every file of a directory, or
//!                                       a file of NUL-separated messages
//!
//! Each target's directory under `<out>` is replaced whole; a file's name is
//! its content's digest.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use fuzz_lanes::frame_lane::seeds as frame_seeds;
use fuzz_lanes::input::{multipart_input, pair_input, proxy_input, turn_input};
use fuzz_lanes::message_lane::seeds as message_seeds;
use fuzz_lanes::proxy_lane::seeds as proxy_seeds;
use fuzz_lanes::turn_lane::{drive, scripts};
use sip_message::header::{HeaderName, MediaType};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

#[path = "../../../crates/b2bua-harness/tests/it/snapshot_lane/seeds.rs"]
mod snapshot_seeds;

/// Inputs by target.
type Corpus = BTreeMap<&'static str, Vec<Vec<u8>>>;

/// The authentication family the credentials target reads.
const CREDENTIALS: [HeaderName; 4] = [
    HeaderName::Authorization,
    HeaderName::ProxyAuthorization,
    HeaderName::WwwAuthenticate,
    HeaderName::ProxyAuthenticate,
];

/// Digest exchanges of RFC 3261 §22 and RFC 2617 §3, which no message seed
/// carries in every form.
const DIGEST: &[&str] = &[
    "Digest realm=\"atlanta.example.com\", qop=\"auth\", nonce=\"84a4cc6f3082121f32b42a2187831a9e\", opaque=\"\", stale=FALSE, algorithm=MD5",
    "Digest username=\"bob\", realm=\"atlanta.example.com\", nonce=\"ea9c8e88df84f1cec4341ae6cbe5a359\", opaque=\"\", uri=\"sips:ss2.biloxi.example.com\", response=\"dfe56131d1958046689d83306477ecc\"",
    "Digest username=\"alice\", realm=\"example.com\", nonce=\"dcd98b7102dd2f0e8b11d0f600bfb0c093\", uri=\"sip:bob@example.com\", qop=auth, nc=00000001, cnonce=\"0a4f113b\", response=\"6629fae49393a05397450978507c4ef1\", opaque=\"5ccc069c403ebaf9f0171e9517f40e41\"",
];

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (out, corpus) = match args.as_slice() {
        [out] => (out, lane_seeds()),
        [out, flag, target] if flag == "--target" => {
            let mut all = lane_seeds();
            match all.remove_entry(target.as_str()) {
                Some(only) => (out, Corpus::from([only])),
                None => {
                    eprintln!("fuzz-seeds: no seeds for target {target}");
                    return ExitCode::from(2);
                }
            }
        }
        [out, flag, path] if flag == "--messages" => match read_messages(Path::new(path)) {
            Ok(messages) => (out, from_messages(&messages)),
            Err(e) => {
                eprintln!("fuzz-seeds: {path}: {e}");
                return ExitCode::FAILURE;
            }
        },
        _ => {
            eprintln!("usage: fuzz-seeds <out> [--target <t> | --messages <path>]");
            return ExitCode::from(2);
        }
    };
    match write(Path::new(out), &corpus) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("fuzz-seeds: {out}: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Every lane's synthetic seeds, in each target's layout.
fn lane_seeds() -> Corpus {
    let fixtures =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crates/sip-message/tests/fixtures");
    let messages: Vec<Vec<u8>> =
        message_seeds::messages_under(&fixtures).into_iter().map(|s| s.bytes).collect();
    let mut corpus = from_messages(&messages);
    let add = |corpus: &mut Corpus, target, input| corpus.entry(target).or_default().push(input);

    let dialled = drive::dialled();
    for (i, (_, script)) in scripts::scripts().iter().enumerate() {
        let run = drive::run(&dialled, script, usize::MAX, &[]);
        for (step, raw) in &run.raws {
            let (Ok(i), Ok(step)) = (u8::try_from(i), u8::try_from(*step)) else { continue };
            add(&mut corpus, "b2bua_turn", turn_input(i, step, raw));
        }
    }
    let descriptions = message_seeds::descriptions();
    for a in &descriptions {
        for b in &descriptions {
            add(&mut corpus, "sdp_answer", pair_input(&a.bytes, &b.bytes));
        }
    }
    for (i, seed) in proxy_seeds::all().iter().enumerate() {
        let Ok(i) = u8::try_from(i) else { continue };
        add(&mut corpus, "proxy_datagram", proxy_input(i, &seed.bytes));
    }
    for seed in frame_seeds::all() {
        add(&mut corpus, "repl_frame", seed.wire);
    }
    for seed in snapshot_seeds::all() {
        add(&mut corpus, "call_snapshot", seed.bytes);
    }
    for line in DIGEST {
        add(&mut corpus, "credentials", line.as_bytes().to_vec());
    }
    corpus
}

/// Raw SIP messages in the layout of each target that takes one: the
/// message itself, its SDP beside the lane's offer and answer, its multipart
/// body under its boundary, its authentication lines, and the datagram the
/// proxy receives (a request from the caller, a response from the worker).
fn from_messages(messages: &[Vec<u8>]) -> Corpus {
    let mut corpus = Corpus::new();
    let mut add = |target, input| corpus.entry(target).or_default().push(input);
    let proxy = proxy_seeds::all();
    let seed_of =
        |name: &str| proxy.iter().position(|s| s.name == name).and_then(|i| u8::try_from(i).ok());
    let parser = CustomParser::new();
    for raw in messages {
        add("message_roundtrip", raw.clone());
        let Ok(msg) = parser.parse(raw) else { continue };
        let (sdp, peer) = match &msg {
            SipMessage::Request(r) => (r.sdp().map(<[u8]>::to_vec), seed_of("req/invite")),
            SipMessage::Response(r) => (r.sdp().map(<[u8]>::to_vec), seed_of("resp/200-worker")),
        };
        if let Some(peer) = peer {
            add("proxy_datagram", proxy_input(peer, raw));
        }
        if let Some(sdp) = sdp {
            add("sdp_answer", pair_input(&sdp, message_seeds::SDP_ANSWER.as_bytes()));
            add("sdp_answer", pair_input(message_seeds::SDP_OFFER.as_bytes(), &sdp));
        }
        if let Some(Ok(media_type)) = msg.header::<MediaType>() {
            if let Some(boundary) = media_type.params().value("boundary") {
                add("multipart", multipart_input(boundary.as_bytes(), msg.body()));
            }
        }
        for h in msg.headers() {
            if CREDENTIALS.contains(&HeaderName::from(h.name.as_str())) {
                add("credentials", h.value.as_str().as_bytes().to_vec());
            }
        }
    }
    corpus
}

/// The messages at `path`: every file of a directory, sorted by name, or
/// the NUL-separated messages of a file.
fn read_messages(path: &Path) -> std::io::Result<Vec<Vec<u8>>> {
    if path.is_file() {
        let all = fs::read(path)?;
        return Ok(all.split(|&b| b == 0).filter(|m| !m.is_empty()).map(<[u8]>::to_vec).collect());
    }
    let mut paths: Vec<PathBuf> =
        fs::read_dir(path)?.map(|e| e.map(|e| e.path())).collect::<Result<_, _>>()?;
    paths.retain(|p| p.is_file());
    paths.sort();
    paths.iter().map(fs::read).collect()
}

fn write(out: &Path, corpus: &Corpus) -> std::io::Result<()> {
    for (target, inputs) in corpus {
        let dir = out.join(target);
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        fs::create_dir_all(&dir)?;
        for input in inputs {
            fs::write(dir.join(format!("{:016x}", fnv1a(input))), input)?;
        }
        println!("{target}: {} inputs", inputs.len());
    }
    Ok(())
}

/// FNV-1a: a stable file name across runs and toolchains.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(0xcbf2_9ce4_8422_2325, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}
