/**
 * The post-run Reclassifier a deployment composes: the escape hatch that refines a
 * structural error into a detailed accept, by RE-READING the finished cell —
 * never by influencing the run.
 *
 * The one refinement it makes: a run the interpreter failed only within what
 * the lane's lists accept — every refused datagram and every absence answered
 * by a named rule, and any `must_fail` declaration the run abandoned before
 * reaching (`./premise.ts` `blessing`) — is restated as passed, and the reason
 * names the rules. Anything else (an unclassified record, a failure the
 * confrontation does not cover, a missing record file) leaves the verdict
 * exactly as the interpreter said it.
 *
 * A SECOND refinement answers the post-run RFC audit rather than the verdict: a
 * gating charge the run can show the SUT only TRANSPORTED drops to recorded
 * (`./rfc-inherit.ts`). A charge that stands keeps the cell red whatever the
 * verdict side says, so no failure bless can green a cell over an RFC gate
 * nobody answered. A scripted party's source fault is not this reading's: it
 * rules the SUT alone, and a source fault is information that fails no cell.
 *
 * A PASSING run is read too and audited (`./audit.ts`): what the audit finds
 * is filed against the RULE, into the sweep's `BlessAudit.Sink`, and the
 * verdict is never touched by it — nor by a sink that cannot be written, which
 * is logged. The one reading that fails a passing run is the body gate
 * (`./bad-body.ts`): a body row no rule accepts, the session description's
 * included.
 *
 * The same reading yields the cell's per-rule counts (`./hits.ts`), returned on
 * `Reclassified.hits` for the driver to write beside the cell.
 */
import { Bundle, Confrontation } from "@sip/contracts"
import { Reclassifier } from "@sip/pipeline"
import * as Effect from "effect/Effect"
import type * as FileSystem from "effect/FileSystem"
import * as Layer from "effect/Layer"
import * as Option from "effect/Option"
import * as BlessAudit from "./audit.js"
import { badBody, failingBody, GATE_HIT, withoutFailingBody } from "./bad-body.js"
import { recordingOf, recordsOf, rfcAuditOf, stepsOf } from "./cell-files.js"
import { cellHits } from "./hits.js"
import { auditFindings, blessing } from "./premise.js"
import * as RfcInherit from "./rfc-inherit.js"

/** The cell's RFC audit findings; none where the cell carries no readable audit. */
const findingsOf = (dir: string): Effect.Effect<ReadonlyArray<Bundle.RfcFinding>, never, FileSystem.FileSystem> =>
  rfcAuditOf(dir).pipe(Effect.map((audit) => RfcInherit.findingsOf(Option.getOrUndefined(audit))))

/**
 * The RFC gate's charges, read back to whoever authored them. The recording is
 * touched only where there is a charge to answer: an audited cell with none is
 * the overwhelming majority, and reading every leg of every one of them would
 * cost a sweep for nothing.
 */
const rfcRuling = (dir: string): Effect.Effect<RfcInherit.Ruling, never, FileSystem.FileSystem> =>
  Effect.gen(function* () {
    const findings = yield* findingsOf(dir)
    return findings.some((f) => f.gating)
      ? RfcInherit.ruling(findings, yield* recordingOf(dir))
      : { ...RfcInherit.NOTHING, relayed: RfcInherit.relayedOf(findings), source: RfcInherit.sourceOf(findings) }
  })

/**
 * The verdict side of the refinement; the counts are added by {@link refine}.
 * A charge the run cannot read back to a peer is the SUT's, and no reading of
 * the verdict side restates a cell over it. A cell neither reading names a
 * reason for stays red: a bless that cannot say what it answered never answers.
 */
const restate = (
  outcome: Reclassifier.RunOutcome,
  records: ReadonlyArray<Confrontation.ConfrontationRecord>,
  rfc: RfcInherit.Ruling
): Reclassifier.Reclassified => {
  if (rfc.charged.length > 0) return { passed: false }
  const failures = outcome.verdict.failures ?? []
  const rules = failures.length === 0
    ? (Confrontation.recordsPass(records) ? [] : undefined)
    : blessing(failures, records, outcome.verdict.abandoned)
  if (rules === undefined) return { passed: false }
  const said: Array<string> = []
  if (failures.length > 0) {
    said.push(`every failure is a difference a named rule classifies (${rules.join(", ")})`)
  }
  if (rfc.inherited.length > 0) {
    said.push(
      `${rfc.inherited.length} RFC charge(s) are a peer's own, relayed unchanged ` +
        `(${RfcInherit.RFC_INHERITED}: ${RfcInherit.rulesOf(rfc.inherited).join(", ")})`
    )
  }
  if (said.length === 0) return { passed: false }
  return { passed: true, reason: `${said.join("; ")} — ok with accepted delta` }
}

/**
 * Whether a cell the run failed passes on `records`: the bless holds on them,
 * or the verdict is clean, no charge stands and every record is answered (a
 * run whose confrontation mode already failed it on those rows).
 */
const passesWithout = (
  outcome: Reclassifier.RunOutcome,
  records: ReadonlyArray<Confrontation.ConfrontationRecord>,
  rfc: RfcInherit.Ruling
): boolean =>
  restate(outcome, records, rfc).passed ||
  (Bundle.verdictPassed(outcome.verdict) && rfc.charged.length === 0 &&
    Confrontation.recordsPass(records))

/**
 * One reading of the cell: the verdict refined, the audit filed, the counts
 * taken. A cell with no readable sheet keeps its verdict and counts nothing.
 */
const refine = (
  sink: BlessAudit.Interface,
  outcome: Reclassifier.RunOutcome
): Effect.Effect<Reclassifier.Reclassified, never, FileSystem.FileSystem> =>
  Effect.gen(function* () {
    const sheet = yield* recordsOf(outcome.dir)
    if (Option.isNone(sheet)) return { passed: outcome.passed }
    const records = sheet.value
    const rfc = yield* rfcRuling(outcome.dir)
    const hits = cellHits(outcome, records, rfc)
    if (outcome.passed) {
      yield* sink.append(auditFindings(outcome, records, rfc)).pipe(
        Effect.catch((error) => Effect.logWarning("bless audit not filed", error))
      )
    }
    const body = failingBody(records).length > 0
    if (outcome.passed && !body) return { passed: true, hits }
    if (!outcome.passed) {
      const restated = restate(outcome, records, rfc)
      if (restated.passed || !body || !passesWithout(outcome, withoutFailingBody(records), rfc)) {
        return { ...restated, hits }
      }
    }
    return { passed: false, reason: badBody(records, yield* stepsOf(outcome.dir)), hits: [...hits, GATE_HIT] }
  })

/**
 * The real Reclassifier: re-reads the cell's files through the `FileSystem` it
 * is composed over and files its audit into the sink.
 */
export const reclassifierLayer: Layer.Layer<
  Reclassifier.Service,
  never,
  BlessAudit.Sink | FileSystem.FileSystem
> = Layer.effect(
  Reclassifier.Service,
  Effect.gen(function* () {
    const sink = yield* BlessAudit.Sink
    const context = yield* Effect.context<FileSystem.FileSystem>()
    return Reclassifier.Service.of({
      reclassify: (outcome) => refine(sink, outcome).pipe(Effect.provideContext(context))
    })
  })
)
