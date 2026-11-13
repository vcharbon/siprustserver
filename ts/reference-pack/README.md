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
| `call-records` | `judges/call-records/` | a module judge over a cell's claim and the records joined to it, here those the simulated lane exported |

## Plug-in and attachment kinds

| Kind | Sample |
|---|---|
| case source | `cases.ts` |
| lane file | `lanes/simulated.toml` |
| judge (module) | `judges/call-records` |
| export | the plain replay binary's `call-records` |
