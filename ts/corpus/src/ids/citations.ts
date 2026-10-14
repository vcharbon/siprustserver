/**
 * What a ticket's text CITES of the store: the runs, jobs, sweeps and preps it
 * names by number, and the case ids it names in any form the resolver reads —
 * `<capture>-<suffix>`, `runN/<case>`, a bare suffix no prose word reads as
 * (`caseN`, `Grammar.bareSuffixes`). Each citation is dated
 * by the earliest text holding it, which is what tells an id of this store
 * from one of a store rebuilt since.
 *
 * Pure: the texts come in (the deployment's `Tracker` reads them), the
 * citations go out. Which suffixes end a case id is the deployment's grammar.
 */
import { type Citations, Resolver } from "@sip/store"

/** One ticket's texts, each with when it was written. */
export interface TicketText {
  readonly ref: string
  readonly texts: ReadonlyArray<{ readonly text: string; readonly on: string }>
}

/** The case-id shapes a text is scanned for, under one grammar's suffixes. */
const shapesOf = (grammar: Resolver.Grammar) => {
  const shapes = Resolver.caseShapes(grammar)
  const suffix = `(?:${shapes.suffixes})`
  return {
    /** A bare suffix, which names a call only with its capture. */
    bare: new RegExp(String.raw`(?<![\w.+…/-])-?(?:${shapes.bareSuffixes})(?![\w+])`, "g"),
    /** `runN/<case>`: a case of the capture run N replayed. */
    runCase: new RegExp(
      String.raw`(?<![\w-])(?:@run:|run)(\d{1,9})/((?:[\w.+…-]*-)?${suffix}|[\w.+…-]+-${suffix})(?![\w+])`,
      "g"
    ),
    /** `<capture>-<suffix>`: a full or elided case id. */
    caseId: new RegExp(String.raw`(?<![\w.+…/-])[A-Za-z0-9_][\w.+…-]*-${suffix}(?![\w+])`, "g")
  }
}

/** One number of a list, with an optional `#` and markdown emphasis or code quotes around it. */
const NUMBER = String.raw`[*\x60]{0,2}#?\d{1,9}[*\x60]{0,2}`

/**
 * What joins two numbers of a list (`,`, `/`, `&`, `|`, `and`, `or`, `vs`,
 * `against`, `→`, a parenthesis) or two ends of a range (`-`, `–`, `..`, `to`,
 * `through`).
 */
const JOIN = String.raw`\s*(?:,|/|&|\||\band\b|\bor\b|\bvs\.?|\bagainst\b|→|->|-|–|\.\.|\bto\b|\bthrough\b|\(|\))\s*`

/** A list cut at its joins, the joins kept. */
const SPLIT = new RegExp(`(${JOIN})`, "i")

/** The joins that name a range. */
const RANGE = /^(-|–|\.\.|to|through)$/

/**
 * `run N`, `runs N, M and P`, `Run #N`, `@run:N`, `sweeps N–M`, `sweep 12 →
 * 15`, `preps from 30 to 34`… in any case: the kind word, then a list of
 * numbers. A run-scoped case (`runN/caseM`) ends the list at its `/`.
 */
const NUMBERED = new RegExp(
  String.raw`(?<![\w-])(?:@(run):|(run|job|sweep|prep)s?\s*(?:\|\s*)?(?:(?:of|from)\s+)?\(?)(${NUMBER}(?:${JOIN}${NUMBER})*)(?![\w])`,
  "gi"
)

/** The widest range a citation is read as every number of; a wider one is read as its two ends. */
const MAX_RANGE = 50

/** Every number a list names: its items, and a short range expanded. */
const numbersOf = (list: string): ReadonlyArray<string> => {
  const out: Array<string> = []
  const parts = list.split(SPLIT)
  for (let at = 0; at < parts.length; at += 2) {
    const value = parts[at]!.replace(/[#*\x60]/g, "").trim()
    const join = (parts[at - 1] ?? "").trim().toLowerCase()
    const previous = Number(out[out.length - 1])
    const current = Number(value)
    if (RANGE.test(join) && current > previous && current - previous <= MAX_RANGE) {
      for (let n = previous + 1; n < current; n++) out.push(String(n))
    }
    out.push(String(current))
  }
  return out
}

/**
 * Every citation the ticket's texts hold, each dated by the earliest text
 * holding it, in first-seen order; case ids read by `grammar`'s suffixes.
 */
export const citationsOf = (
  ticket: TicketText,
  grammar: Resolver.Grammar
): ReadonlyArray<Citations.Citation> => {
  const shapes = shapesOf(grammar)
  const found = new Map<string, Citations.Citation>()
  const note = (kind: Citations.Kind, ref: string, on: string): void => {
    const day = on.slice(0, 10)
    const key = `${kind}\u0000${ref}`
    const held = found.get(key)
    if (held === undefined) found.set(key, { kind, ref, citedBy: ticket.ref, citedOn: day })
    else if (held.citedOn !== null && day < held.citedOn) found.set(key, { ...held, citedOn: day })
  }
  for (const { on, text } of ticket.texts) {
    for (const match of text.matchAll(NUMBERED)) {
      const kind = (match[1] ?? match[2]!).toLowerCase() as Citations.Kind
      for (const ref of numbersOf(match[3]!)) note(kind, ref, on)
    }
    // A run-scoped case is read whole, then masked, so its case is not read again as a bare one.
    let rest = text
    for (const match of text.matchAll(shapes.runCase)) {
      note("case", match[0], on)
      rest = rest.replace(match[0], " ".repeat(match[0].length))
    }
    for (const match of rest.matchAll(shapes.caseId)) note("case", match[0], on)
    for (const match of rest.matchAll(shapes.bare)) note("case", match[0], on)
  }
  return [...found.values()]
}
