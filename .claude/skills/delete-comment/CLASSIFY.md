# Classify every comment block of one slice of a crate

Your prompt names the repository root (read only), `SLICE`, `OUT` and `ADRS`. The repository is
public: everything you write is public too.

You classify; you edit nothing in the repository and build nothing. The only files you write are
your output files (below). Read each source file whole (the `Read` tool, in parts if large)
before you classify its blocks: you judge each comment against the code it sits on.

## Input

`SLICE` (named in your prompt): one JSON line per comment block, with `id` (`<file>:<start>`),
`file` (relative to the repository), `start`/`end` (1-based lines), `kind` (`outer-doc` `///`,
`inner-doc` `//!`, `line` plain `//`), `trailing` (code before it on its line), `banned` (true
for every plain comment) and `text`.

The house rule this sweep enforces: a comment is the doc comment of a function, type, field or
module, and states its **contract** (behaviour, invariants, preconditions, units, error cases,
the RFC point the signature does not show). Every in-body plain `//` comment is banned, except a
one-line `// SAFETY:` or a bare one-line ADR pointer (`// ADR-0051 §3`). History (dates, commit
hashes, ticket ids, "previously", "replaces", "no longer", "moved from") lives in git and ADRs.

## Categories (one per block)

- `tautology`: repeats what the code already shows (the name, the signature, the type, the next
  line). `/// Returns the port.` on `fn port()`; `// increment the counter`; a test doc that
  restates the test name.
- `stale`: names code, behaviour or history that no longer holds or no longer matters (a removed
  function, an old design, a ticket or migration note, "previously …"). Say what no longer holds,
  with the `file:line` you checked.
- `excuse`: justifies a hacky, partial or wrong implementation so it can stay: "for now",
  "workaround", "best effort", "we don't handle X", "simplification", "ideally …", "TODO",
  a silently swallowed error explained away, a known RFC departure without a decision behind it.
  The hack is real code you can point at.
- `decision`: records an important, non-obvious design decision (why this structure, why this
  trade-off, why not the obvious alternative), worth keeping for the next person.
- `contract`: a doc comment that states a contract the code does not show. For a plain in-body
  comment, `contract` means a contract of the enclosing item that its doc comment lacks.

A block mixing kinds is classified by its most valuable part, with `rewrite` (below) cutting the
rest.

## Actions

- `delete`: the block goes. Every `tautology` and `stale`, every `excuse`, and every `decision`
  once its content is in an ADR. A plain in-body comment is deleted unless it is `to-doc` or
  `adr-pointer`.
- `keep`: a `contract` doc comment kept verbatim.
- `rewrite`: a doc comment kept, its text replaced by `text` (complete replacement lines, same
  indentation and `///`/`//!` prefix): the contract alone, present tense, no history, no
  restating of the signature. A `decision` whose ADR exists may keep a one-line `See ADR-00xx.`
  inside a contract doc comment this way.
- `to-doc` (plain in-body comments only, rare): its content is a contract of the enclosing item
  that the item's doc comment lacks. Give `insert_before` (the line number before which new doc
  lines go: the item's first attribute or keyword line, below its existing doc comment) and `text`
  (the doc lines, indented, `///`).
- `adr-pointer` (plain comments only): the block becomes a bare one-line `// ADR-00xx[ §n]`
  pointer (no text after it), given as `text`. Only where a reader at this exact line needs the
  ADR.

## Output fields (one JSON line per input block, every id of the slice exactly once)

`id`, `category`, `action`, and as they apply:

- `text`: for `rewrite`, `to-doc`, `adr-pointer` (`\n`-joined lines, exact indentation).
- `insert_before`: for `to-doc`.
- `why`: for `stale` (what no longer holds, `file:line`), and one short clause for any
  `rewrite`.
- `hack`, `where`, `fix`, `criticity`: for `excuse`. `hack` is one sentence naming the hacky
  behaviour in code terms; `where` the `file:line` of that code; `fix` one sentence on the
  correct implementation; `criticity` `low` (cosmetic), `normal` (a rare case, or tooling that
  slows or misleads), `high` (SIP correctness, call cleanup or failover wrong in a case
  production will meet), `critical` (calls fail in ordinary traffic).
- `adr`, and for a new one `topic` and `decision`: for `decision`. First check
  `docs/adr/` (list in `ADRS`, read the likely ones): when an existing ADR already holds the
  decision, `adr` is its file name and nothing else is needed. When none does, `adr` is
  `"new"`, `topic` a short kebab-case slug grouping related decisions (reuse your own slugs
  across blocks: `cancel-lru`, `hrw-stickiness`, `health-probing`, …), and `decision` 2–5
  sentences: the context, the decision, the alternative rejected and why, the consequence.
  When an existing ADR holds part of it, `adr` is that file and `decision` the missing part.

All text you write (rewrites, decisions, fixes) is public: generic SIP and RFC terms only, no
customer, product or business names, fictitious examples (555 numbers, RFC 5737/3849
addresses). Rewrites follow the house style: present tense, no hedging, no history.

## How to judge

- Read the code. A doc comment on a `pub` item that states what a caller must know (when it
  fails, what it guarantees, what an argument means beyond its type, which RFC rule it applies)
  is `contract`. A field doc that only expands the field name is `tautology`.
- A module doc (`//!`) usually mixes the module's contract with design narrative: keep the
  contract (`rewrite`), send the narrative that is a real decision to `decision`.
- A test's doc comment stating the scenario and what it proves is `contract`; one naming a
  ticket or narrating a past bug is `stale` or a `rewrite`.
- Lean on `delete`: when in doubt between `tautology` and `contract`, ask whether a competent
  reader of the signature and body would miss anything without it.
- Be strict on `excuse`: a sentence that explains away a limit is an excuse even when politely
  worded. Do not invent hacks; the code must show the behaviour the comment excuses.

## Writing your output

Write one output file per source file as you finish it, so the work is durable:
`OUT/<the source path with / replaced by __>.jsonl` (`OUT` named in your prompt), with the
`Write` tool. No other file.

## Final message (under 40 lines)

Counts per category and per action; each `excuse` in one line (`where`: `hack`); each new
`decision` topic in one line; any block you could not classify and why.
