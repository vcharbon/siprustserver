/**
 * The cells a case source the pack names yields (`@sip/rig` `CaseSource`), as
 * the runner runs them: such a cell holds no lock, needs no capability, is
 * replayed once and holds nothing after it; its checks are its document's,
 * and the operations it asks for are its document's steps'.
 *
 * Its `call-records` claim is stated in the document it runs
 * (`postconditions.claims`), which the judge answers over the records the
 * system wrote after the call; a cell stating it nowhere, or both beside and
 * in its document, is refused. A claim on any other collector, which no part
 * of the lane answers, is not requested: the cell that passes is skipped
 * naming it, never a pass, and a batch where such a collector is `required`
 * is refused (`claims.ts`).
 */
import { Postcondition } from "@sip/contracts"
import { type CompiledCell, OperationStep } from "@sip/rig"
import * as Result from "effect/Result"
import type { Cell, Checked } from "./cell.js"
import { checkedOf } from "./checked.js"
import { unansweredOf } from "./claims.js"

const { CALL_RECORDS } = Postcondition

/** A source cell as the simulated lane runs it: its document stating the `call-records` claim, and the collectors of its claims no lane part answers. */
export interface Answered {
  readonly compiled: CompiledCell.CompiledCell
  readonly notRequested: ReadonlyArray<string>
}

/** Whether `document` states the `call-records` claim: in its `claims`, or as a `cdr` count. */
const claimsRecords = (document: CompiledCell.CompiledCell["document"]): boolean => {
  const post = document.postconditions
  return post?.claims?.[CALL_RECORDS] !== undefined || (post?.cdr !== undefined && "count" in post.cdr)
}

/**
 * `compiled` with its `call-records` claim stated in its document's
 * `postconditions.claims`, with the collectors of its other claims, stated in
 * its document or beside it ({@link unansweredOf}); or why it is refused: a
 * cell stating no `call-records` claim either way, or one stating it both in
 * its document and beside it. The claim's shape is the pivot schema lint's
 * to refuse.
 */
export const answeredOf = (compiled: CompiledCell.CompiledCell): Result.Result<Answered, string> => {
  const claim = compiled.claims[CALL_RECORDS]
  const notRequested = unansweredOf(compiled.document, compiled.claims)
  const stated = claimsRecords(compiled.document)
  if (claim === undefined) {
    return stated
      ? Result.succeed({ compiled, notRequested })
      : Result.fail(`${compiled.id}: states no ${CALL_RECORDS} claim: the simulated lane counts every call's records`)
  }
  if (stated) return Result.fail(`${compiled.id}: states its ${CALL_RECORDS} claim both in its document and beside it`)
  const post = compiled.document.postconditions
  const postconditions = { ...post, claims: { ...post?.claims, [CALL_RECORDS]: claim } }
  return Result.succeed({ compiled: { ...compiled, document: { ...compiled.document, postconditions } }, notRequested })
}

/** Where a source cell runs and is filed: its directory, its file in the run, its tags, its egress endpoint, and the gates it runs after its steps' checks. */
export interface Placing {
  readonly dir: string
  readonly source: Cell["source"]
  readonly tags: ReadonlyArray<string>
  readonly alone: boolean
  readonly egress: string
  readonly gates: ReadonlyArray<Checked>
}

/**
 * The runner's cell for the source cell `answered`, placed as `placing` says,
 * asking for the operations its document's steps inject; a pack's cell adds
 * its own fields beside it.
 */
export const cellOfSource = ({ compiled, notRequested }: Answered, placing: Placing): Cell => {
  const operations = OperationStep.operationsOf(compiled.document)
  return {
    id: compiled.id,
    ref: compiled.case,
    subcase: compiled.sub_case ?? "",
    retries: 0,
    tags: placing.tags,
    alone: placing.alone,
    heal: { healMs: 0, healRealClockOnSimulatedMs: 0 },
    source: placing.source,
    checked: checkedOf(compiled.document, placing.gates),
    locks: [],
    needs: [],
    dir: placing.dir,
    endpoints: compiled.document.endpoints.map((e) => e.id),
    egress: placing.egress,
    notRequested,
    ...(operations.length === 0 ? {} : { operations })
  }
}
