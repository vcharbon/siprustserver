# E2E documents

The authored JSON documents of the e2e axis data model (`crates/e2e-model`,
ADR-0018/0019/0021) that the load generator reads, and their generated schemas.

```
e2e/
  cases/         Test cases: input data, binding pools, checks, compatible shapes
  checksets/     Shared check bundles a Test case references by id
                 (`loadgen --check-sets-dir`, default `e2e/checksets`)
  loadprofiles/  Load profiles: the whole loadgen run spec (`loadgen --load-profile`)
  schemas/       Generated JSON Schemas; an authored file names one in `$schema`
```

A Test case is attached to a load mix entry (`loadgen --case`, or `case=` in a
`--scenario` entry); its checks bind `<agent>.<anchor>` blocks to the anchors
the shape publishes, validated at load. `crates/loadgen/README.md` is the
how-to.

The schemas are generated from the model types; a drift test in `e2e-model`
fails until they are regenerated:

```sh
cargo run -p xtask -- e2e-schema
```
