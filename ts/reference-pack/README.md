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

## Plug-in and attachment kinds

| Kind | Sample |
|---|---|
| case source | `cases.ts` |
| lane file | `lanes/simulated.toml`, `lanes/reference.toml` |
| part schema and part file | `parts/b2bua-runner.schema.json`, `targets/b2bua-runner/reference.toml` |
| judge (module) | `judges/call-records` |
| collector (batch scope) | `collectors/sut-log` |
| export | the plain replay binary's `call-records` |
| attachment `log` | `sut-log` |
