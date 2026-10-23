/**
 * What a `--case` list comes to, before anything is planned: every entry read
 * back to a case id of ONE capture, in any form a ticket cites it — a bare
 * suffix of the named capture (`case110`), a full case id, its capture named
 * by its stem, `runN/caseN`, a Call-ID — and that capture named where the line
 * named none. An entry that names no call of it is refused with the reason:
 * the cut's refusal tokens and why, an ambiguous suffix's candidates, the
 * accepted forms.
 */
import { IdForms } from "@sip/corpus"
import type { Selection } from "@sip/corpus/universe"
import { Resolver } from "@sip/store"
import * as Effect from "effect/Effect"

/** The selection with its cases read, or why it cannot be. */
export type Read =
  | { readonly _tag: "read"; readonly selection: Selection }
  | { readonly _tag: "refused"; readonly why: string }

/** How many candidates an ambiguous entry names. */
const CANDIDATES = 5

/** Why a refusal token was minted, in one line (`CliFacts.refusalWhy`). */
export type Why = (token: string) => string

/** One entry the resolver did not read to a single call, as the lines that say why. */
const unread = (resolution: Resolver.Resolution, why: Why): string => {
  const head = `--case ${resolution.input}: ${resolution.status}: ${resolution.note}`
  if (resolution.status === "refused") {
    return [head, ...resolution.calls.flatMap((call) => call.reasons.map((r) => `  ${r}: ${why(r)}`))]
      .join("\n")
  }
  if (resolution.status === "ambiguous") {
    const shown = resolution.calls.slice(0, CANDIDATES).map((call) => `  ${call.caseId}`)
    const more = resolution.total > shown.length ? [`  … ${resolution.total - shown.length} more`] : []
    return [head, ...shown, ...more].join("\n")
  }
  return head
}

/** One entry's capture and case id, or why it names none. */
const entryOf = Effect.fn("CaseSelection.entryOf")(function*(named: string | undefined, entry: string, why: Why) {
  const forms = yield* IdForms.caseFormsOf
  const inCapture = named === undefined ? entry.trim() : forms.caseIdIn(named, entry)
  const capture = forms.captureOf(inCapture)
  if (named !== undefined && capture === named) return { capture, caseId: inCapture }
  const resolver = yield* Resolver.Service
  const resolution = yield* resolver.resolve(inCapture)
  const call = resolution.calls[0]
  if (resolution.status !== "resolved" || call === undefined) return unread(resolution, why)
  return { capture: call.capture, caseId: call.caseId }
})

/**
 * The selection's `--case` entries read to case ids of one capture, that
 * capture the positional where the line named none; unchanged where it named
 * no case. The capture rules of a cases run (`Population.refusal`) still apply
 * to what this returns.
 */
export const resolve = Effect.fn("CaseSelection.resolve")(function*(selection: Selection, why: Why) {
  if (selection.cases.length === 0) return { _tag: "read", selection } satisfies Read
  const named = selection.positional.length === 1 ? selection.positional[0] : undefined
  const read = yield* Effect.forEach(selection.cases, (entry) => entryOf(named, entry, why))
  const refused = read.filter((one): one is string => typeof one === "string")
  if (refused.length > 0) return { _tag: "refused", why: refused.join("\n") } satisfies Read
  const entries = read as ReadonlyArray<{ readonly capture: string; readonly caseId: string }>
  const captures = [...new Set(entries.map((one) => one.capture))]
  if (named !== undefined && captures.some((capture) => capture !== named)) {
    const elsewhere = entries.filter((one) => one.capture !== named).map((one) => `${one.caseId} (${one.capture})`)
    return {
      _tag: "refused",
      why: `--case names calls of ${named} alone; these name another capture: ${elsewhere.join(", ")}`
    } satisfies Read
  }
  if (captures.length > 1) {
    return {
      _tag: "refused",
      why: `--case names the calls of one capture; these name ${captures.length}: ${captures.join(", ")}`
    } satisfies Read
  }
  return {
    _tag: "read",
    selection: {
      ...selection,
      positional: selection.positional.length === 0 ? captures : selection.positional,
      cases: [...new Set(entries.map((one) => one.caseId))]
    }
  } satisfies Read
})
