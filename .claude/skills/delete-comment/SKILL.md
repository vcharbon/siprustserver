---
name: delete-comment
description: Sweep the comments of a crate (or a directory of Rust sources) - classify each as tautology, stale, excuse, decision or contract, delete what goes, move decisions into ADRs, file the hacks the excuses covered, and put the crate under the comment ban. Use when asked to delete, scrub, audit or classify comments, or to run the comment sweep on a crate.
argument-hint: "crates/<name> [crates/<other> ...]"
---

The sweep leaves each crate's comments as doc comments that state contracts, an ADR for each
decision, an issue for each hack, and a comment-only diff. The rule it enforces is the
repository's comment ban (`scripts/comments.py --help`): a comment is a doc comment stating
its item's contract; every other comment is banned except a lone one-line `// SAFETY:` or ADR
pointer.

The tool is `comments.py` beside this file (`--help`). Every path below is relative to the
repository root; `T=.claude/skills/delete-comment/comments.py`.

## Steps

1. **Measure.** `$T estimate <crate>...`: the lines each crate would shed with no plain
   comments, no doc comments, and none at all, for src and tests. Keep the output for the
   report. Done when every crate has its four rows.

2. **Inventory.** `$T list <crate>... > <scratch>/inv.jsonl`, run from the root. Split it by
   file into slices of about 300 blocks or 25 files, a file never split across slices.

3. **Classify.** One read-only agent per slice, all at once, each given
   [CLASSIFY.md](CLASSIFY.md) as its brief, its `SLICE`, an `OUT` directory of its own, and
   the `ADRS` list (`ls docs/adr`). The agents edit nothing; you are the one editor. Done when
   every inventory id appears exactly once across the `OUT` files: check it with a short
   script, and send a slice's missing ids back to a new agent.

4. **Review the calls.** Read every `excuse`, every `decision`, every `to-doc` and every
   `rewrite` text: each must name real code, be in present tense, and read as public content:
   generic terms, no private names. Fix a row in the `OUT` file. Read every `delete` row of a
   doc comment on a `pub` item: a deleted contract is the costly mistake, and an `excuse` mixed
   with a contract (a function's `None` cases, say) is a `rewrite` keeping the contract. A
   rewrite links only public items from a public item's doc.

5. **Record decisions.** Group the `decision` rows by `topic`. A row naming an existing ADR
   whose text lacks its `decision` gets that text added to the ADR. Each topic with `adr:
   new` becomes one ADR, numbered past the highest in `docs/adr/`, in that directory's
   format, its Owner the files the rows came from. A row the ADR now covers is `delete`, or an
   `adr-pointer` where a reader at that line needs it.

6. **File the excuses.** Bundle the `excuse` rows by the hack they cover (one issue per hack,
   or per area for small ones), each issue naming the `where` lines, the `hack` and the `fix`,
   at its worst row's criticity. Search the tracker first; an open issue on the same hack
   takes a comment instead. On a box with its own workflow, that workflow names the tracker.

7. **Apply.** `$T apply <OUT dirs>...` from the root, then `cargo fmt --all`. Apply refuses the
   whole run when an id no longer matches the source, so apply before any other edit to the
   crates.

8. **Prove.** All three must hold:
   - `$T same-code -C . --base <base> <crate>...` prints `0 with code changes`;
   - the crates' tests pass with unchanged counts, and `cargo doc` of the crates has no new
     warning (a deleted doc comment can leave an intra-doc link dangling);
   - `$T list <crate>...` shows only doc comments and the allowed one-line forms.

9. **Activate the ban.** Add one `scan <crate dir>` line per swept crate to `comments.clean`,
   then `scripts/comments.py` passes: the crate is held to no banned comment from now on. A
   crate never leaves the list.

10. **Report.** The counts per category and action, the step 1 estimate against the lines the
    diff actually removed (`git diff --stat`), the ADRs written or extended, and the issues
    filed with their criticity.
