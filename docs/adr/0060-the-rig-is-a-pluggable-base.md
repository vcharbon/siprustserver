# The rig is a pluggable base

**Status:** proposed

**Owner:** `ts/` (the site server, the campaign runner, the lifecycle, the store), `crates/`
(the replay library, `pivot-interpreter`, `pivot-schema`), `tooling/siprig/` (the image kit and
the reference pack).

Manifests, protocol lines, the cell description and every table are in the
[appendix](appendix/0060-the-rig-is-a-pluggable-base.md), under the same item numbers.

## Context

The capture replay, the campaign and the site showing their results are one tool with no name.
A product can use it only by owning most of it. The viewer app has no server here. The corpus
runs shell lines around a replay job, and they can only annotate. The bundle contract fixes the
file names of what a call leaves. The interpreter waits for a call's records before the call
settles, so every call pays for the slowest record source. Two campaign runners exist. A second
product would write its own server, runner, lane files and image.

## Decision

The tool is **siprig**, "the rig": a base that a product extends with a pack and a lab extends
with a workspace, through one contract. Every seam is cut for a product the base does not know.

1. **Three levels.** The *base* is this repository's. A *pack* is one product's content, in its
   image. A *test bed* is one workspace, owned by whoever runs the lab. It builds nothing. What
   a test bed can define, a pack can define the same way.
2. **One rig path.** The pack, then the workspace. Each holds `collectors/`, `judges/`,
   `operations/` and `parts/`. A name defined twice is refused, naming both files. A manifest
   is TOML with a schema version, validated before anything runs. Data is the contract and an
   executable in any language is the leaf.
3. **Three scopes.** A job (one launched replay or campaign), a batch (the cells a lane runs
   between a stand and a drain) and a cell (one case on one lane). A replay and a campaign go
   through one lifecycle.
4. **A cell claims, exports and returns.** A case states *claims*: per collector name, what it
   expects. The interpreter does not wait for a claim. The cell ends when its call ends, with
   the claim's verification pending. What only the cell's process knows, it *exports* as
   records under a collector's name.
5. **A collector reads the lab.** It has `collect`, and optionally `open` before its scope and
   `close` after it, sharing one state directory. `close` always runs, is idempotent and
   restores what the collector changed. A collector may return records, answer verifications,
   attach files, add keys to a cell, hold named locks and name the collectors it runs `after`.
   A cell-scope collector may state a delay. It then runs that long after the cell ended, and
   the lane is not held.
6. **The base joins, a judge decides.** The records of a name are those its collector returned
   and those the cells exported. The base joins them to cells by equality on the one key the
   manifest names, in one hash pass. A collector may assign its records itself. The judge of
   that name receives every cell of the scope with its claim and its records, and answers. It
   is a TypeScript module or an executable.
7. **An operation changes the lab.** It has `inject`, `heal` and optionally `probe`, with the
   rules of ADR-0049 §2 and its own runner. A named campaign lists operations as its `setup`. A
   case names one as a step of its call. A lane file binds the name a case asks for.
8. **Verifications decide a cell.** A verification is a named line with a source and an
   answer: `pass`, `fail`, `not checkable` or `pending`. The call assertions, the RFC audit,
   the backend double, the in-process seam and every collector or judge answer their own. A
   collector or judge that fails or times out answers `not checkable`. The final status is
   derived from the lines.
9. **Attachments are typed.** A file attached to a job, a batch or a cell has a kind: `log`,
   `records`, `diff`, `table`, `json`, `file` or `link`. The site renders each kind with one
   component. A pack ships no frontend code.
10. **Wiring.** A collector states the lane parts it needs and its default: `always`,
    `on-failure` or `on-demand`. It runs on every lane that has those parts. A lane file states
    only what differs. The workspace file defines collection levels that override the defaults
    for one job. Nothing overrides a collector marked `required`.
11. **A lane is a SIP target and parts.** The base reads the target, the clock, the backend
    mode, the pacing and the tags. Every other property is a part: a named file several lanes
    may share, with a schema a pack or a workspace declares.
12. **On a simulated lane no external collector runs at cell scope.** A cell there is one
    process on a paused clock. Its verifications are in process, behind the registration seam.
13. **The replay is a library with a registration seam.** A product registers what must be in
    process: its backend double, its simulated system under test, its in-process verifications
    and its exports. It builds its own binary. The base ships one with nothing registered.
14. **The base accepts compiled cells from any case source.** It ships a minimal case form and
    two shapes, `basic_call` and `invite_reject`. A product's case vocabulary is its pack's.
15. **One site and one runner.** The site server, the campaign runner and the store's result
    tables are the base's. Every page shows `siprig`, the pack's title and the build stamp.
    This replaces the runner and the site of ADR-0018. `e2e-model` stays as ADR-0021 has it.
16. **The image is built from a kit.** The base image holds the binaries, the site, node and a
    store, and no Python. A pack states what its image adds and builds it from the base one.
17. **A reference pack exercises every seam.** It has no provisioning and no test value: one
    sample per plug-in kind, a call from alice to bob, one capture recorded here. This
    repository's checks run it.
18. **Pack and workspace code is trusted as its owner.** There is no sandbox.

## Considered and rejected

- **Frontend plug-ins.** The costliest seam. Typed attachments cover a call record from any
  vendor.
- **One runner with the HA tooling, or an adapter for its incidents.** They are YAML, source a
  shell library and select pods. The rules are shared, nothing else.
- **A check and a gatherer with different authority.** One kind with its own line does both.
- **A `missing` answer.** Whoever looked states `fail` or `not checkable`.
- **Collectors that declare the operations they use.** A second orchestration layer.
- **A lane scope.** A job-scope collector runs once per part instance of the job's lanes.
- **Splitting a product's shape ladder to share it.** Two shapes written here cost less.
- **Waiting for records inside the call.** One slow source then delays every cell.

## Consequences

- A wait that follows a call is a number in a manifest or a lane file.
- A case that claims on a lane where no collector and no export can answer is told so: a
  `required` collector refuses the batch, another one is *not requested* and the job says it.
- The result tables are created anew by the release that brings verifications. The frozen case
  ids, the population and the cuts are kept.
- The cut and comparison seams of the pipeline are unchanged by this decision.
