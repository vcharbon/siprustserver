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
| `restart-worker` | `operations/restart-worker/`, `lanes/reference.toml` (`[operations]`) | an operation needing the `b2bua-runner` and `runner-pid` parts, so a lane naming neither part, or only one, cannot bind it: `inject` stops the runner the `runner-pid` part's file names (the base names there the runner it stands, while it runs) with the `signal` of its `[params]` and awaits its exit, its orchestrator, the base on this lane, standing the next one; it refuses to signal a process unless the runner answering `GET /admin/cordon` at the `b2bua-runner` part's `admin` states its readiness and that pid (a stale pid file may name another process); `heal` ends a stop an interrupted inject left, after the same check, `probe` answers once the stopped runner is gone. Named in a run's setup (`siprig-campaign run --setup restart-worker`), it stops a runner an earlier job left on the lane's addresses before the job's first batch stands its own; the lane binds it by name for a case to ask for |
| `capture` | `collectors/capture/`, `parts/sip-capture.schema.json`, `targets/sip-capture/reference.toml` | a job collector, `on-failure`: `open` starts the job's capture, emptying the file the runner captures to (`B2BUA_CAPTURE`, the lane's `sip-capture` part) before the job's first batch stands it, so the file holds one job's capture at most; a box-wide lock keeps every other job's capture off it; `collect` runs only where a cell failed and attaches to each failed cell its calls' packets as a `file` attachment, a pcapng of its own, and a `table` of them, a row per packet |
| the reference capture | `capture/forked-reliable.ts`, `capture/forked-reliable.pcapng` | a capture recorded through the upstream B2BUA and replayed: ingested with the identity extraction, cut by the base's case generator and replayed as a case source (see below) |
| `calls-ended` | `collectors/calls-ended/` | a delayed cell collector: `delay_s` after each call it lists the runner's live calls (`GET /admin/calls` at the part's `admin`), while the lane runs its next cells (the lane not held), and answers the cell's `calls-ended` per call (`pass` where none of the cell's Call-IDs is listed, `fail` where one is, whatever the other cells' calls); it attaches what it read as `json` and where it read it as a `link` |

## Plug-in and attachment kinds

| Kind | Sample |
|---|---|
| case source | `cases.ts`, `capture/forked-reliable.ts` (a flow no shape plays), a cut's output directory (`--sources`) |
| capture ingest (identity extraction), cut (base generator), replay | `capture/forked-reliable.pcapng`, `test/reference-replay.test.ts` |
| claim | `cases.ts`, `call-records` |
| lane file | `lanes/simulated.toml`, `lanes/reference.toml` |
| part schema and part file | `parts/<kind>.schema.json` and `targets/<kind>/reference.toml` for each of `b2bua-runner`, `runner-log`, `runner-pid`, `sip-capture`, `cdr-file` |
| judge (module), over a collector's records and over exports | `judges/call-records` |
| collector (batch scope, a judge, `[join] key`, `[params]`, a lock) | `collectors/call-records` |
| collector (job scope, `on-failure`, a lock) | `collectors/capture` |
| collector (batch scope, bracket, state directory, a lock) | `collectors/sut-log` |
| collector (cell scope, delayed, the lane not held) | `collectors/calls-ended` |
| operation (`inject`, `heal`, `probe`, `[params]`, a part it needs), a run's setup, a lane's `[operations]` | `operations/restart-worker` |
| export | the plain replay binary's `call-records` |
| attachment `log` | `sut-log` |
| attachment `file` | `capture` |
| attachment `table` | `capture` |
| attachment `json` | `calls-ended` |
| attachment `link` | `calls-ended` |
| attachment `records` | the `call-records` judge |
| attachment `diff` | the `call-records` judge |

Not shown, since each would add a process start to every hosted batch: a
pre-check (a collector with `at = "before"`, ADR-0060 appendix §3), a collector's `close` step
(§5; `sut-log` empties its log in `collect` instead) and an executable judge (`run`, §6; the
`call-records` judge is a module).

## The reference capture

`capture/forked-reliable.pcapng` is one call recorded through the upstream B2BUA. The case
`capture/forked-reliable.ts` has Alice call Bob, whose side forks the INVITE. Each fork rings
reliably (RFC 3262) on an early dialog of its own, the first answers, and the second's early
dialog ends with the INVITE's transaction (RFC 3261 §13.2.2.4).

- **Recording**: the base's campaign run plays the case (`--sources`) on the reference lane, and
  the runner's own capture (the `sip-capture` part) is the recording. The parties sit on
  127.0.0.2 and the runner on 127.0.0.1, since no documentation address binds unprivileged and
  the cut tells the system from its parties by address. `just reference-capture` records it
  again (`test/reference-capture.test.ts` with `REFERENCE_CAPTURE_WRITE=1`); without the variable
  the same test runs in the checks and asserts the wire.
- **Replay** (`test/reference-replay.test.ts`, in the checks): the capture is ingested with the
  identity extraction (`@sip/corpus` `ExtractionIdentity`: the kept flows document is the raw
  one, under no seed), cut by the base's case generator (`@sip/corpus` `cut/base-generator.ts`,
  the neutral policy, the runner's legs joined by its `counter-prefix` Call-ID derivation) into
  one case, and replayed by the base's campaign run on the simulated lane as its one cell, which
  passes. The callee is claimed by arrival order, since its R-URI names no number.
