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
| the simulated lane | `lanes/simulated.toml` | a lane file; the plain replay binary, which exports the worker's record of each call (`exports/call-records.jsonl`) |
| the reference lane | `lanes/reference.toml`, `targets/b2bua-runner/reference.toml`, `parts/b2bua-runner.schema.json` | a real-clock lane the base hosts: the upstream B2BUA runner stood per batch on loopback, its addresses and log a part's, the cells in the plain binary's batch process (`pivot-replay batch`), each call's b-leg pinned to its callee by the dial's `X-Api-Call` |
| `call-records` (judge) | `judges/call-records/` | a module judge over each cell's claim and the records joined to it: those the `call-records` collector read on the reference lane, those the plain binary exported on the simulated one; it attaches each claiming cell its records flattened (`records`) and its claim against them (`diff`) |
| `call-records` (collector) | `collectors/call-records/`, `parts/cdr-file.schema.json`, `targets/cdr-file/reference.toml` | a batch collector naming a judge: `open` empties the file the runner appends each call's record to (`B2BUA_CDR_FILE`, the lane's `cdr-file` part), `collect` waits up to `settle_s` (`[params]`) for a record of each cell's call and answers each record keyed by its a-leg's Call-ID, which the base joins on `call_id` (`[join] key`); a box-wide lock keeps every other batch's collector off the file |
| `sut-log` | `collectors/sut-log/` | a batch collector bracketing its batch (`open` notes the runner's log length in its state directory, `collect` reads what the batch appended), the batch's log and each cell's share of it as `log` attachments; an executable with no library, run by node with its types stripped |
| `capture` | `collectors/capture/`, `parts/sip-capture.schema.json`, `targets/sip-capture/reference.toml` | a job collector, `on-failure`: `open` starts the job's capture, emptying the file the runner captures to (`B2BUA_CAPTURE`, the lane's `sip-capture` part) before the job's first batch stands it, so the file holds one job's capture at most; a box-wide lock keeps every other job's capture off it; `collect` runs only where a cell failed and attaches to each failed cell its calls' packets as a `file` attachment, a pcapng of its own, and a `table` of them, a row per packet |
| `calls-ended` | `collectors/calls-ended/` | a delayed cell collector: `delay_s` after each call it reads the runner's `b2bua_active_calls`, while the lane runs its next cells (the lane not held), and answers the cell's `calls-ended` (`pass` at none, `not-checkable` otherwise: the gauge is the worker's, not the call's); it attaches the reading as `json` and where it read it as a `link` |

## Plug-in and attachment kinds

| Kind | Sample |
|---|---|
| case source | `cases.ts` |
| claim | `cases.ts`, `call-records` |
| lane file | `lanes/simulated.toml`, `lanes/reference.toml` |
| part schema and part file | `parts/b2bua-runner.schema.json`, `targets/b2bua-runner/reference.toml`, `parts/sip-capture.schema.json`, `targets/sip-capture/reference.toml`, `parts/cdr-file.schema.json`, `targets/cdr-file/reference.toml` |
| judge (module), over a collector's records and over exports | `judges/call-records` |
| collector (batch scope, a judge, `[join] key`, `[params]`, a lock) | `collectors/call-records` |
| collector (job scope, `on-failure`, a lock) | `collectors/capture` |
| collector (batch scope, bracket, state directory) | `collectors/sut-log` |
| collector (cell scope, delayed, the lane not held) | `collectors/calls-ended` |
| export | the plain replay binary's `call-records` |
| attachment `log` | `sut-log` |
| attachment `file` | `capture` |
| attachment `table` | `capture` |
| attachment `json` | `calls-ended` |
| attachment `link` | `calls-ended` |
| attachment `records` | the `call-records` judge |
| attachment `diff` | the `call-records` judge |
