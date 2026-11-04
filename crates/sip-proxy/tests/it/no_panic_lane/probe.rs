//! The health probe reading a worker's OPTIONS answer (ADR-0050 §3, §8): on
//! UDP anything can answer from a worker's address, so whatever header lines
//! that answer carries — its `X-Overload` load report, `X-Readiness`,
//! `X-Incarnation` — the probe's loop and the proxy routing on what it read
//! keep turning. A case answers the probe's first round, worker a with the
//! case's status and lines, worker b plainly, then routes the caller's INVITE
//! on what the probe read and cancels it.

use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::{CustomParser, Method, SipMessage, SipParser, SipRequest};

use sip_proxy::load_observer::EluBand;

use super::rig::{runtime, Outcome, Peer, Rig, Variant, GAP};
use super::seeds::{caller_cancel, caller_invite};

/// The statuses a case answers the probe with, by the case's status byte.
pub const STATUSES: [u16; 4] = [200, 200, 503, 486];

/// What a worker's answer reports by default: low load, new calls accepted,
/// its first boot.
pub const REPORT: &[u8] =
    b"X-Overload: v=1; elu=0.20; gc=0.01; adm=12\r\nX-Readiness: new=accept\r\nX-Incarnation: 1\r\n";

/// The lines the seeds answer with: the default report, a loaded and
/// refusing worker that restarted, and a report of another version.
pub fn seed_lines() -> Vec<&'static [u8]> {
    vec![
        REPORT,
        b"X-Overload: v=1; elu=0.97; gc=0.30; adm=4\r\nX-Readiness: new=refuse; cause=draining\r\nX-Incarnation: 2\r\n",
        b"X-Overload: v=2; elu=0.5\r\n",
    ]
}

fn options(raw: &[u8]) -> Option<SipRequest> {
    match CustomParser::new().parse(raw) {
        Ok(SipMessage::Request(r)) if *r.method() == Method::Options => Some(r),
        _ => None,
    }
}

/// The answer `status` to `req` carrying `lines` (CRLF-terminated header
/// lines), as a worker's UDP datagram.
fn answer(req: &SipRequest, status: u16, lines: &[u8]) -> Option<Vec<u8>> {
    let opts = GenerateResponseOpts { to_tag: Some("w".into()), ..Default::default() };
    let image = generate_response(req, status, "Probe", &opts).ok()?.image().to_vec();
    let head = image.strip_suffix(b"\r\n")?;
    Some([head, lines, b"\r\n"].concat())
}

/// What a probe case left: its outcome, and the band the observer put
/// worker a in.
pub struct Probed {
    pub outcome: Outcome,
    pub band: Option<EluBand>,
}

/// One case: worker a answers the probe `status` with `lines`.
pub fn case(status: u16, lines: &[u8]) -> Probed {
    runtime().block_on(async {
        let rig = Rig::with(Variant::Probed).await;
        tokio::time::sleep(GAP).await;
        for (worker, status, lines) in
            [(Peer::WorkerA, status, lines), (Peer::WorkerB, 200, REPORT)]
        {
            for raw in rig.drain(worker) {
                let Some(reply) = options(&raw).and_then(|req| answer(&req, status, lines)) else {
                    continue;
                };
                rig.send_to_probe(worker, &reply).await;
            }
        }
        rig.send(Peer::Caller, &caller_invite()).await;
        let forwarded = [Peer::WorkerA, Peer::WorkerB, Peer::Caller]
            .iter()
            .map(|p| rig.drain(*p).len())
            .sum::<usize>();
        let band = rig.observer.band_for("b2b-a");
        rig.send(Peer::Caller, &caller_cancel()).await;
        rig.run_past_sweeps().await;
        Probed { outcome: Outcome { failure: rig.failure(), forwarded }, band }
    })
}
