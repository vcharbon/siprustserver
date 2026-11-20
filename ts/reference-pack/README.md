# The reference pack

A pack of the rig (ADR-0060 item 17) that exercises its seams on the upstream B2BUA. It has no
provisioning and no test value of its own: each sample is the smallest one that shows its seam,
documentation that runs. Its names, numbers and addresses are those the RFCs reserve for
examples.

It lives in the `ts/` workspace so its case source resolves `@sip/basic-case`. Its campaign runs
in this repository's checks (`pnpm -r test`, `test/reference-campaign.test.ts`) and from
`just reference-campaign`, through the base's campaign run (`@sip/siprig` `base-campaign.ts`,
the image's `siprig-campaign run`).

| Sample | Where | Seam it exercises |
|---|---|---|
| `alice-calls-bob`, `bob-is-busy` | `cases.ts` | the minimal case form (`@sip/basic-case`) and both shapes, `basic_call` and `invite_reject`; a case source (`[cases] sources`); a claim, `call-records`, in the case form's `{ count, checks }`: one record of the call, stating how the callee answered and how the call ended |
| `alice-calls-bob-through-a-restart`, `alice-calls-bob-after-the-restart` | `step-cases.ts` (named by `--sources`) | an operation as a step of a call: once Bob received the ACK, `restart-worker` SIGKILLs the runner (the reference lane's `[operations.restart-worker]` `params`), the base stands the next one, which answers each party's BYE 481 (`forgets`), and no record is written; the cell heals, then the next call runs on the runner stood again. Both carry the tag `restarts`, which the run's workspace declares `alone` (`rig.toml`), or the source is refused at compile |
| the simulated lane | `lanes/simulated.toml` | a lane file; the plain replay binary, which exports the worker's record of each call (`exports/call-records.jsonl`) |
| the reference lane | `lanes/reference.toml`, `targets/b2bua-runner/reference.toml`, `parts/b2bua-runner.schema.json` | a real-clock lane the base hosts: the upstream B2BUA runner stood per batch on loopback, its addresses a part's and each file it writes a part's of its own (`runner-log`, `runner-pid`, `sip-capture`, `cdr-file`), the cells in the plain binary's batch process (`pivot-replay batch`), each call's b-leg pinned to its callee by the dial's `X-Api-Call` |
| `call-records` (judge) | `judges/call-records/` | a module judge over each cell's claim and the records joined to it: those the `call-records` collector read on the reference lane, those the plain binary exported on the simulated one; it attaches each claiming cell its records flattened (`records`) and its claim against them (`diff`) |
| `call-records` (collector) | `collectors/call-records/`, `parts/cdr-file.schema.json`, `targets/cdr-file/reference.toml` | a batch collector naming a judge: `open` empties the file the runner appends each call's record to (`B2BUA_CDR_FILE`, the lane's `cdr-file` part), `collect` waits up to `settle_s` (`[params]`) for a record of each cell's call and answers each record keyed by its a-leg's Call-ID, which the base joins on `call_id` (`[join] key`); a box-wide lock keeps every other batch's collector off the file |
| `sut-log` | `collectors/sut-log/` | a batch collector bracketing its batch (`open` notes the runner's log length in its state directory, `collect` reads what the batch appended, then empties the log, so it holds one batch's lines at most), the batch's log and each cell's share of it as `log` attachments; a box-wide lock keeps every other batch's collector off the log; an executable with no library, run by node with its types stripped |
| `restart-worker` | `operations/restart-worker/`, `lanes/reference.toml` (`[operations]`) | an operation needing the `b2bua-runner` and `runner-pid` parts, so a lane naming neither part, or only one, cannot bind it: `inject` stops the runner the `runner-pid` part's file names (the base names there the runner it stands, while it runs) with the `signal` of its `[params]` and awaits its exit, its orchestrator, the base on this lane, standing the next one; it refuses to signal a process unless the runner answering `GET /admin/cordon` at the `b2bua-runner` part's `admin` states its readiness and that pid (a stale pid file may name another process); `heal` ends a stop an interrupted inject left, after the same check, `probe` answers once the stopped runner is gone and, as a case's step, once another runner answers at `admin`. It `stops` the system: its inject names the runner it stopped in `SIPRIG_OUT/stopped`, the one death the base claims as the step's. Named in a run's setup (`siprig-campaign run --setup restart-worker`), it stops a runner an earlier job left on the lane's addresses before the job's first batch stands its own; the lane binds it by name for a case to ask for |
| `capture` | `collectors/capture/`, `parts/sip-capture.schema.json`, `targets/sip-capture/reference.toml` | a job collector, `on-failure`: `open` starts the job's capture, emptying the file the runner captures to (`B2BUA_CAPTURE`, the lane's `sip-capture` part) before the job's first batch stands it, so the file holds one job's capture at most; a box-wide lock keeps every other job's capture off it; `collect` runs only where a cell failed and attaches to each failed cell its calls' packets as a `file` attachment, a pcapng of its own, and a `table` of them, a row per packet |
| the reference capture | `capture/forked-reliable.ts`, `capture/forked-reliable.pcapng` | a capture recorded through the upstream B2BUA and replayed: ingested with the identity extraction, cut by the base's case generator and replayed as a case source (see below) |
| the recorded samples | `capture/answered-call.ts`, `capture/{basic,emergency,reinvite}-call.{ts,pcapng}` | three more captures recorded and replayed the same way: a basic answered call, an emergency call and a call held by a re-INVITE, between parties with display names and numbers (see below) |
| the vendored captures | `capture/third-party/` | two captures another stack produced, ingested and cut but not replayed: a proxied blind transfer, cut into a case, and a B2BUA's calls the default leg rules join, each cut into a case (see below) |
| `calls-ended` | `collectors/calls-ended/` | a delayed cell collector: `delay_s` after each call it lists the runner's live calls (`GET /admin/calls` at the part's `admin`), while the lane runs its next cells (the lane not held), and answers the cell's `calls-ended` per call (`pass` where none of the cell's Call-IDs is listed, `fail` where one is, whatever the other cells' calls); it attaches what it read as `json` and where it read it as a `link` |

`test/call-facts-parity.test.ts` holds each case document against the capture it was cut from,
on the call facts both sides state.

## The reference capture

`capture/forked-reliable.pcapng` is one call recorded through the upstream B2BUA. The case
`capture/forked-reliable.ts` has Alice call Bob, whose side forks the INVITE. Each fork rings
reliably (RFC 3262) on an early dialog of its own, the first answers, and the second's early
dialog ends with the INVITE's transaction (RFC 3261 §13.2.2.4).

- **Recording**: the base's campaign run plays the case (`--sources`) on the reference lane, and
  the runner's own capture (the `sip-capture` part) is the recording. The parties sit on
  127.0.0.2 and the runner on 127.0.0.1, since no documentation address binds unprivileged and
  the cut tells the system from its parties by address. `just reference-capture` records it
  again with the recorded samples below (`test/reference-capture.test.ts` with
  `REFERENCE_CAPTURE_WRITE=1`); without the variable the same test runs in the checks and asserts
  the wire.
- **Replay** (`test/reference-replay.test.ts`, in the checks): the capture is ingested with the
  identity extraction (`@sip/corpus` `ExtractionIdentity`: the kept flows document is the raw
  one, under no seed), cut by the base's case generator (`@sip/corpus` `cut/base-generator.ts`,
  the neutral policy, the runner's legs joined by its `counter-prefix` Call-ID derivation and
  the default leg rules) into
  one case, and replayed by the base's campaign run on the simulated lane as its one cell, which
  passes. The callee is claimed by arrival order, since its R-URI names no number.

## The recorded samples

Three more captures are recorded and replayed as the reference capture is, each by its own case
source, each written to its own file by the same test. The cases share one call
(`capture/answered-call.ts`): Alice Example (`+15555550100`) calls Bob Example
(`+15555550101`), Bob rings and answers, and Alice hangs up. Each party's display name and
number ride its From, To and Contact on the caller's leg; the system writes the numbers, not the
display names, on the callee side's From and To.

| Capture | Case | What it shows |
|---|---|---|
| `basic-call.pcapng` | `basic-call.ts` | the answered call ended by BYE |
| `emergency-call.pcapng` | `emergency-call.ts` | the same, its INVITE carrying `Resource-Priority: esnet.0` (RFC 4412), which the system carries to the callee side |
| `reinvite-call.pcapng` | `reinvite-call.ts` | the same, held by the caller's re-INVITE before the BYE: a sendonly offer answered recvonly (RFC 3264 §8.4), CSeq 2 on both legs |

`test/reference-capture.test.ts` asserts each capture's ladder on both legs, and
`test/reference-replay.test.ts` cuts each into one case keeping what it shows and replays it.

## The vendored captures

`capture/third-party/` holds two captures copied unchanged from another MIT-licensed repository;
`capture/third-party/PROVENANCE.md` states their source, SHA-256 and licence notice.
`test/third-party-captures.test.ts` asserts the hashes, then ingests and cuts each under the
base's default leg rules (`@sip/corpus` `data/default.rules.json`). Neither is replayed.

- `refer-transfer.pcap`: two calls through a record-routing proxy, each with a blind transfer
  (RFC 3515). The completed one is cut into a case; the one the capture leaves open is refused.
- `asterisk-22-xcid.pcap`: three calls through a B2BUA that gives the callee's leg a Call-ID of
  its own and names the caller's in an `X-CID` header. The default rule set's one `header-pair`
  rule joins the two legs; under a rule file stating no rule (`--correlation-rules`), no call
  is cut.
