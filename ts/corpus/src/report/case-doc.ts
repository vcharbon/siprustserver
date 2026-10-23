/**
 * ONE case's document, by any cited form of its id (`Resolver`), read at the
 * cut that names it — the capture's newest prep naming it, or the prep asked
 * for — and printed as one of:
 *
 *   path    the case directory, materialised, its document and its side rows
 *   flow    the reference ladder: the capture's legs the document selects,
 *           as the text API draws them
 *   ledger  the side rows the case carries for the interpreter's peers, one a line
 *
 * An id no reading names (a frozen id alone, a refusal, an ambiguous suffix)
 * is refused with the resolver's reason.
 */
import { Cuts, Resolver } from "@sip/store"
import * as Effect from "effect/Effect"
import * as path from "node:path"
import * as Content from "../content/content.js"
import { documentFile } from "../cut/case-dir.js"
import { CaseLayout } from "../deployment/index.js"
import * as IdForms from "../ids/id-forms.js"
import * as CallEvidence from "./call-evidence.js"
import { referenceLadder, selectedLegs, storedFlows } from "./call-ladders.js"

export const PRINTS = ["path", "flow", "ledger"] as const

export type Print = (typeof PRINTS)[number]

export interface Input {
  /** The capture the id is a case of, where the line named one. */
  readonly capture?: string
  readonly id: string
  /** The prep to read the case as of; the newest prep naming it where absent. */
  readonly prep?: number
  readonly print: Print
}

export type Read =
  | { readonly _tag: "lines"; readonly lines: ReadonlyArray<string> }
  | { readonly _tag: "refused"; readonly why: string }

const refused = (why: string): Read => ({ _tag: "refused", why })

/** The case `input` names: its capture, id and the prep it is read at, or why there is none. */
const caseOf = Effect.fn("CaseDoc.caseOf")(function*(input: Input) {
  const forms = yield* IdForms.caseFormsOf
  const asked = input.capture === undefined ? input.id : forms.caseIdIn(input.capture, input.id)
  const resolver = yield* Resolver.Service
  const resolution = yield* resolver.resolve(asked)
  const call = resolution.calls[0]
  if (resolution.status !== "resolved" || call === undefined) {
    return `${asked}: ${resolution.status}: ${resolution.note}`
  }
  const prepId = input.prep ?? call.prepId
  if (prepId === null) return `${call.caseId}: ${resolution.note}`
  const cuts = yield* Cuts.Service
  const standing = yield* cuts.standing(call.capture, prepId)
  const entry = standing?.cases.find((one) => one.caseId === call.caseId)
  if (standing === null || entry === undefined) return `${call.caseId}: no case of the cut of ${call.capture} as of prep${prepId}`
  return { capture: call.capture, caseId: call.caseId, prepId: standing.prepId, flowsHash: standing.flowsHash, caseHash: entry.caseHash }
})

/** One case's document printed as `input.print` asks, materialised under `sessions`. */
export const read = Effect.fn("CaseDoc.read")(function*(sessions: string, input: Input) {
  const found = yield* caseOf(input)
  if (typeof found === "string") return refused(found)
  const head = `${found.caseId}  prep${found.prepId}  ${found.capture}`
  const cuts = yield* Cuts.Service
  if (input.print === "path") {
    const dir = yield* Content.materialise(sessions, found.caseHash)
    const layout = yield* CaseLayout.Service
    return {
      _tag: "lines",
      lines: [
        head,
        `case      ${dir}`,
        `document  ${path.join(dir, documentFile(found.caseId))}`,
        `ledger    ${path.join(dir, layout.rowsDir)}`
      ]
    } satisfies Read
  }
  const content = yield* cuts.content(found.caseHash)
  if (content === null) return refused(`${found.caseId}: the store holds no content for case ${found.caseHash}`)
  if (input.print === "ledger") {
    return {
      _tag: "lines",
      lines: [head, ...content.ledger.map((row) => `${row.number}  ${JSON.stringify(row.document)}`)]
    } satisfies Read
  }
  if (found.flowsHash === null) return refused(`${found.caseId}: the cut of prep${found.prepId} stored no flows document`)
  const envelope = yield* CallEvidence.storedEnvelope(found.flowsHash)
  if (envelope === null) return refused(`${found.caseId}: the flows document ${found.flowsHash} does not read`)
  const flows = yield* storedFlows(envelope, selectedLegs(content.document))
  const ladder = referenceLadder(flows, content.document, `${found.caseId} (reference)`)
  if (ladder === undefined) return refused(`${found.caseId}: the document is no v3 pivot, or the flows document does not draw`)
  return { _tag: "lines", lines: [head, ...ladder] } satisfies Read
})
