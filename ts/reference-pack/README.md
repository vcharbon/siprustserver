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
| `alice-calls-bob`, `bob-is-busy` | `cases.ts` | the minimal case form (`@sip/basic-case`) and both shapes, `basic_call` and `invite_reject`; a case source (`[cases] sources`) |
| the simulated lane | `lanes/simulated.toml` | a lane file; the plain replay binary, which exports the worker's record of each call (`exports/call-records.jsonl`) |
| the reference lane | `lanes/reference.toml`, `targets/b2bua-runner/reference.toml`, `parts/b2bua-runner.schema.json` | a real-clock lane the base hosts: the upstream B2BUA runner stood per batch on loopback, its addresses and log a part's, the cells in the plain binary's batch process (`pivot-replay batch`), each call's b-leg pinned to its callee by the dial's `X-Api-Call` |
| `call-records` | `judges/call-records/` | a module judge over a cell's claim and the records joined to it, here those the simulated lane exported |
| `sut-log` | `collectors/sut-log/` | a batch collector bracketing its batch (`open` notes the runner's log length, `collect` reads what the batch appended), the batch's log and each cell's share of it as `log` attachments; an executable with no library, run by node with its types stripped |
| `capture` | `collectors/capture/`, `parts/sip-capture.schema.json`, `targets/sip-capture/reference.toml` | a job collector, `on-failure`: `open` starts the job's capture, keeping in its state directory where the file the runner captures to (`B2BUA_CAPTURE`, the lane's `sip-capture` part) ends; `collect` runs only where a cell failed and attaches to each failed cell its calls' packets as a `file` attachment, a pcapng of its own |
| `calls-ended` | `collectors/calls-ended/` | a delayed cell collector: `delay_s` after each call it reads the runner's `b2bua_active_calls`, while the lane runs its next cells (the lane not held), and answers the cell's `calls-ended` (`pass` at none, `not-checkable` otherwise: the gauge is the worker's, not the call's) |

## Plug-in and attachment kinds

| Kind | Sample |
|---|---|
| case source | `cases.ts` |
| lane file | `lanes/simulated.toml`, `lanes/reference.toml` |
| part schema and part file | `parts/b2bua-runner.schema.json`, `targets/b2bua-runner/reference.toml`, `parts/sip-capture.schema.json`, `targets/sip-capture/reference.toml` |
| judge (module) | `judges/call-records` |
| collector (job scope, `on-failure`, state directory) | `collectors/capture` |
| collector (batch scope, bracket) | `collectors/sut-log` |
| collector (cell scope, delayed, the lane not held) | `collectors/calls-ended` |
| export | the plain replay binary's `call-records` |
| attachment `log` | `sut-log` |
| attachment `file` | `capture` |
