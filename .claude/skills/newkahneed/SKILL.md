---
name: upstreamneed
description: Process pending requirement files from ~/upstreamneed/ — read them all, run ONE consolidated clarification round upfront, then implement / double-review (opus + fable) / test / commit each item; workflow when several files, direct when one.
---

# Processing ~/upstreamneed/ requirement items

Each `~/upstreamneed/*.md` file is one requirement/defect item. Marker suffixes
(before `.md`) encode state:

- **pending** — no marker (also treat `-PARTIALDONE` / `-DONE-PARTIAL` as
  done unless the user names them explicitly).
- `-DONE-OK` / `-DONE` — completed.
- `-DONE-REJECT` — rejected, with the explanation appended inside the file.

If the user passed arguments, process only the named item(s); otherwise every
pending file.

## Phase 1 — read everything before touching anything

1. List `~/upstreamneed/`, select the pending files, and **read every selected
   file in full** before asking a single question or writing any code.
2. For each item, verify the premise against the current code (grep/read the
   cited seams). Items are sometimes stale — the code they describe may have
   been deleted or already fixed (see 042 for a precedent).
3. Cross-check each requirement against CLAUDE.md rules, the linked docs
   (test-clock, harness-layers, observability, ADR-0022/0014) and against the
   other pending items.

## Phase 2 — ONE consolidated clarification round (all questions upfront)

Before any implementation, present a single consolidated batch covering ALL
items (use AskUserQuestion — multiple calls are fine, but all of them happen
now, none mid-implementation):

- **Clarifications**: every ambiguity, per item.
- **Better / more generic alternatives**: where a simpler, more general, or
  more idiomatic-for-this-repo way exists to reach the requirement, propose it
  with a recommendation and let the user pick.
- **Inconsistencies**: every contradiction — within an item, between items, or
  between an item and a repo invariant/ADR — must be surfaced here and
  resolved. Do not silently pick a side.
- **Stale premises**: items whose premise no longer holds → propose rejection.

Record the decisions; they are the spec for Phase 3. Items the user rejects
here are finalized immediately: append a `## REJECTED — <reason>` section to
the file, then rename it to `<stem>-DONE-REJECT.md`. Only a true blocker
discovered during implementation justifies a later question.

## Phase 3 — execution

**Several files** → orchestrate with the Workflow tool (this skill is the
user's explicit opt-in). **Exactly one file** (or one survivor after
rejections) → skip the workflow and run the same per-item pipeline directly in
the main loop, but the double review below is still mandatory — spawn the two
reviewer agents even for a single item.

### Hard concurrency rule (WSL2)

Never more than ONE agent at a time that compiles or runs tests. Therefore:
items are processed **strictly sequentially** (a `for` loop with `await`, never
`pipeline()`/`parallel()` across items). The two reviewers are read-only
(explicitly forbidden from running cargo) and may run in parallel with each
other, but never concurrently with an implementing/testing agent. Cap heavy
commands per CLAUDE.md (`systemd-run --user --scope -q -p MemoryMax=12G
-p CPUQuota=1200% nice -n 10 cargo … --jobs 6`).

### Per-item pipeline (identical in workflow and direct mode)

1. **Implement** the requirement per the Phase-2 decisions. Follow CLAUDE.md
   and read the matching docs first (test-clock before any timed test, etc.).
   If implementation uncovers that the item must be rejected after all, stop
   and finalize it as `-DONE-REJECT` with the explanation.
2. **Double review — mandatory, two reviewers**: spawn two independent
   reviewer agents on the uncommitted diff, one with `model: 'opus'` and one
   with `model: 'fable'`. Each gets the requirement file content, the Phase-2
   decisions, and the diff; each reviews for correctness vs the requirement,
   repo-standards compliance (CLAUDE.md, observability rules, sip-message
   extraction boundary), and test adequacy. Reviewers must not build or run
   anything.
3. **Address every remark**: fix it, or record a one-line justification for
   not fixing. No remark is dropped silently.
4. **End-to-end SIP callflow test**: add at least one test that models a
   complete callflow exercising the change (`B2buaScene` / scenario_harness
   per CLAUDE.md; RFC-audited, properly terminated, `assert_fully_reaped`
   where applicable), in addition to any unit tests.
5. **Test**: run the touched crates' tests, then the default lane
   (`just test`). Green is required before commit.
6. **Commit**: one commit per item on the current branch, conventional style
   (`feat(scope): …` / `fix(scope): …`), mentioning the item number
   (e.g. `upstreamneed-065`).
7. **Finalize the file**: rename to `<stem>-DONE-OK.md`; if deliberately
   partial, append the explanation of what remains and rename to
   `<stem>-DONE-PARTIAL.md`.

### Workflow shape (several items)

Keep the orchestration script sequential per item, reviews fanned out inside:

```js
export const meta = {
  name: 'upstreamneed-batch',
  description: 'Sequentially implement clarified upstreamneed items with opus+fable double review',
  phases: [{ title: 'Implement' }, { title: 'Review' }, { title: 'Finalize' }],
}
const out = []
for (const item of args.items) {           // sequential: one builder at a time
  const impl = await agent(implementPrompt(item), { phase: 'Implement', label: `impl:${item.id}` })
  const reviews = await parallel([          // read-only reviewers only
    () => agent(reviewPrompt(item, impl), { model: 'opus',  phase: 'Review', label: `review-opus:${item.id}` }),
    () => agent(reviewPrompt(item, impl), { model: 'fable', phase: 'Review', label: `review-fable:${item.id}` }),
  ])
  out.push(await agent(finalizePrompt(item, reviews), { phase: 'Finalize', label: `final:${item.id}` }))
}
return out
```

Each agent prompt must be self-contained: inline the requirement file content,
the Phase-2 decisions for that item, and (for review/finalize) the prior
stage's findings — agents start with no conversation context. The finalize
agent addresses remarks, adds the callflow test, runs the tests, commits, and
renames the file.

## Reporting

End with a per-item table: item → outcome (DONE-OK / DONE-PARTIAL /
DONE-REJECT), commit hash, review remarks addressed vs justified-skipped, and
the callflow test added.
