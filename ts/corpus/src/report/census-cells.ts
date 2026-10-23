/**
 * The cells a census rule's hits name (a replay `--from-census <file> --rule
 * R`). A hit names a capture, the call group of its flows document and a
 * leg's Call-ID; a case document states the call groups it was cut from
 * (`case.source.call_groups`), so a hit lands on the case of its capture's
 * standing cut whose groups hold its group, else on the case keyed by its
 * Call-ID. The calls of a split family (`<root>#ingress@µs`) share their
 * group and their root Call-ID, so between siblings the hit's leg decides:
 * the sibling whose document replays that leg of the flows document, and
 * every sibling holding the group where none does. A hit no case holds is
 * said so, never dropped: its capture was not cut, or the cut refused or
 * never proposed that call.
 *
 * Group and leg numbers index ONE flows document, so the document each hit
 * names must be the one the capture's cut read: its bytes hash to the cut's
 * `flows_hash`. The census names its document by path, so this proves the
 * file at that path now; a capture whose file is gone or differs refuses the
 * whole invocation, naming it.
 */
import { CaseDocuments, Cuts } from "@sip/store"
import { captureSelection } from "@sip/viewer-core/flows"
import type { PivotV3 } from "@sip/viewer-core/pivot"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Stream from "effect/Stream"
import * as crypto from "node:crypto"

/** The head of a census hit this reads. */
export interface Hit {
  readonly capture: string
  /** The flows document the census read, by path. */
  readonly document?: string
  readonly group: number
  readonly leg: number
  readonly call_id: string
  readonly rule: string
}

/** One case of a capture's standing cut, with the call groups its document states. */
export interface CutCall {
  readonly caseId: string
  readonly callKey: string
  readonly groups: ReadonlyArray<number>
  /** The flows document's legs the case replays; read where a sibling shares its group or root. */
  readonly legs?: ReadonlyArray<number>
}

/** One capture's cells: the case ids the hits name, in cut order. */
export interface CaptureCells {
  readonly capture: string
  readonly cases: ReadonlyArray<string>
}

/** A hit no case holds: its capture has no standing cut, or no case of the cut holds its call. */
export interface Unmatched {
  readonly capture: string
  readonly group: number
  readonly callId: string
  readonly why: "no-cut" | "no-case"
}

export interface Cells {
  readonly work: ReadonlyArray<CaptureCells>
  /** Every hit no case holds, in report order. */
  readonly unmatched: ReadonlyArray<Unmatched>
}

/** The root Call-ID a call key names: the key before any `#` qualifier. */
const rootOf = (callKey: string): string => callKey.split("#")[0]!

/** The cells `rule`'s hits name in `calls` (each capture's standing cases), captures in report order. */
export const cellsOf = (
  hits: ReadonlyArray<Hit>,
  rule: string,
  calls: ReadonlyMap<string, ReadonlyArray<CutCall>>
): Cells => {
  const named = new Map<string, Set<string>>()
  const unmatched: Array<Unmatched> = []
  for (const hit of hits) {
    if (hit.rule !== rule) continue
    const cases = calls.get(hit.capture)
    const missed = (why: Unmatched["why"]): Unmatched => ({ capture: hit.capture, group: hit.group, callId: hit.call_id, why })
    if (cases === undefined) {
      unmatched.push(missed("no-cut"))
      continue
    }
    const grouped = cases.filter((one) => one.groups.includes(hit.group))
    const candidates = grouped.length > 0 ? grouped : cases.filter((one) => rootOf(one.callKey) === hit.call_id)
    if (candidates.length === 0) {
      unmatched.push(missed("no-case"))
      continue
    }
    const onLeg = candidates.filter((one) => one.legs?.includes(hit.leg) === true)
    const held = named.get(hit.capture) ?? new Set<string>()
    for (const found of onLeg.length > 0 ? onLeg : candidates) held.add(found.caseId)
    named.set(hit.capture, held)
  }
  const work = [...named].map(([capture, held]) => ({
    capture,
    cases: calls.get(capture)!.map((one) => one.caseId).filter((id) => held.has(id))
  }))
  return { work, unmatched }
}

/**
 * The unmatched hits as a few lines: the captures with no standing cut, by
 * name, and per cut capture how many hits no case holds, the first `shown`
 * of them by group and Call-ID.
 */
export const unmatchedLines = (unmatched: ReadonlyArray<Unmatched>, shown = 3): ReadonlyArray<string> => {
  const uncut = [...new Set(unmatched.filter((one) => one.why === "no-cut").map((one) => one.capture))]
  const byCapture = new Map<string, Array<Unmatched>>()
  for (const one of unmatched) {
    if (one.why !== "no-case") continue
    byCapture.set(one.capture, [...(byCapture.get(one.capture) ?? []), one])
  }
  const named = uncut.slice(0, shown).join(", ") + (uncut.length > shown ? ` (+${uncut.length - shown} more)` : "")
  return [
    ...(uncut.length === 0 ? [] : [`${uncut.length} capture(s) with hits have no standing cut: ${named}`]),
    ...[...byCapture].map(([capture, missed]) =>
      `${capture}: ${missed.length} hit(s) no case of the cut holds (refused or never proposed), e.g. ` +
      missed.slice(0, shown).map((one) => `group ${one.group} ${one.callId}`).join(", ")
    )
  ]
}

/** Why a report cannot answer `rule`: it never measured it, naming the rules it did. */
export const ruleRefusal = (report: { readonly rules: Readonly<Record<string, unknown>> }, rule: string): string | null => {
  const measured = Object.keys(report.rules).sort()
  return measured.includes(rule)
    ? null
    : `the census report never measured "${rule}": it measured ${measured.join(", ")}`
}

/** The hits and the measured rules of a census report; why it does not read, where it does not. */
export const readReport = Effect.fn("CensusCells.readReport")(function*(file: string) {
  const fs = yield* FileSystem.FileSystem
  return yield* fs.readFileString(file).pipe(
    Effect.flatMap((text) => Effect.try(() => JSON.parse(text) as { rules?: Record<string, unknown>; hits?: ReadonlyArray<Hit> })),
    Effect.map((report) => ({ rules: report.rules ?? {}, hits: report.hits ?? [] })),
    Effect.catch((error) => Effect.succeed(`the census report ${file} does not read: ${String(error)}`))
  )
})

/** One capture's flows documents: the one the census read (its bytes' hash; null where it is gone), and the cut's. */
export interface FlowsReading {
  readonly capture: string
  readonly document: string
  readonly read: string | null
  readonly cut: string | null
}

/** Why the hits cannot be read against the cuts: the captures whose census document is not the cut's, or null. */
export const flowsRefusal = (readings: ReadonlyArray<FlowsReading>): string | null => {
  const off = readings.filter((one) => one.read === null || one.read !== one.cut)
  if (off.length === 0) return null
  return [
    `${off.length} capture(s) were cut from another flows document than the census read; their group and leg ` +
    "numbers name other calls (re-run the census, or re-cut):",
    ...off.map((one) =>
      `  ${one.capture}: ${one.document} ` +
      (one.read === null ? "is gone" : `hashes ${one.read.slice(0, 12)}, the cut read ${one.cut?.slice(0, 12) ?? "none"}`)
    )
  ].join("\n")
}

/** Where a case document states the call groups it was cut from, and its flow. */
const GROUPS_FIELD = ["case", "source", "call_groups"]
const FLOW_FIELD = ["flow"]

const groupsOf = (value: unknown): ReadonlyArray<number> =>
  Array.isArray(value) ? value.filter((group): group is number => typeof group === "number") : []

/** The flows document's legs a case's flow replays. */
const legsOf = (flow: unknown): ReadonlyArray<number> =>
  Array.isArray(flow) ? [...captureSelection({ flow: flow as PivotV3["flow"] }).legs] : []

/** The cases that share a group or a root Call-ID with another case: the siblings the hit's leg must part. */
const siblings = <A extends { readonly callKey: string; readonly groups: ReadonlyArray<number> }>(
  calls: ReadonlyArray<A>
): ReadonlyArray<A> => {
  const byKey = new Map<string, number>()
  const keys = (one: A) => [...one.groups.map((group) => `g${group}`), `r${rootOf(one.callKey)}`]
  for (const one of calls) for (const key of keys(one)) byKey.set(key, (byKey.get(key) ?? 0) + 1)
  return calls.filter((one) => keys(one).some((key) => (byKey.get(key) ?? 0) > 1))
}

/**
 * Each capture's standing cases as of `prepId`, with their call groups and,
 * for siblings, their legs; and the flows document each cut read. A capture
 * with no standing cut is absent from both.
 */
export const callsOf = Effect.fn("CensusCells.callsOf")(function*(captures: ReadonlyArray<string>, prepId: number) {
  const cuts = yield* Cuts.Service
  const documents = yield* CaseDocuments.Service
  const calls = new Map<string, ReadonlyArray<CutCall>>()
  const flows = new Map<string, string | null>()
  for (const capture of captures) {
    const standing = yield* cuts.standing(capture, prepId)
    if (standing === null || standing.broke !== null) continue
    const groups = yield* documents.fieldOfCases(standing.cases.map((one) => one.caseHash), GROUPS_FIELD)
    const grouped = standing.cases.map((one) => ({ ...one, groups: groupsOf(groups.get(one.caseHash)) }))
    const shared = siblings(grouped)
    const flowOf = yield* documents.fieldOfCases(shared.map((one) => one.caseHash), FLOW_FIELD)
    calls.set(
      capture,
      grouped.map((one) => ({
        caseId: one.caseId,
        callKey: one.callKey,
        groups: one.groups,
        ...(flowOf.has(one.caseHash) ? { legs: legsOf(flowOf.get(one.caseHash)) } : {})
      }))
    )
    flows.set(capture, standing.flowsHash)
  }
  return { calls, flows }
})

/** The sha-256 of a file's bytes, read as a stream; null where it does not read. */
const fileHash = Effect.fn("CensusCells.fileHash")(function*(file: string) {
  const fs = yield* FileSystem.FileSystem
  const digest = crypto.createHash("sha256")
  return yield* fs.stream(file).pipe(
    Stream.runForEach((chunk) => Effect.sync(() => digest.update(chunk))),
    Effect.map((): string | null => digest.digest("hex")),
    Effect.orElseSucceed(() => null)
  )
})

/** Each cut capture's census document beside the flows document its cut read. */
const flowsReadings = Effect.fn("CensusCells.flowsReadings")(function*(
  hits: ReadonlyArray<Hit>,
  flows: ReadonlyMap<string, string | null>
) {
  const named = new Map<string, string>()
  for (const hit of hits) if (flows.has(hit.capture) && hit.document !== undefined) named.set(hit.capture, hit.document)
  return yield* Effect.forEach([...named], ([capture, document]) =>
    Effect.map(fileHash(document), (read): FlowsReading => ({ capture, document, read, cut: flows.get(capture) ?? null })))
})

/** The cells a census report file's `rule` names as of `prepId`, or why it names none. */
export const read = Effect.fn("CensusCells.read")(function*(file: string, rule: string, prepId: number) {
  const report = yield* readReport(file)
  if (typeof report === "string") return report
  const refused = ruleRefusal(report, rule)
  if (refused !== null) return refused
  const hits = report.hits.filter((hit) => hit.rule === rule)
  const captures = [...new Set(hits.map((hit) => hit.capture))]
  if (captures.length === 0) return `the census report holds no hit of "${rule}"`
  const { calls, flows } = yield* callsOf(captures, prepId)
  const mismatch = flowsRefusal(yield* flowsReadings(hits, flows))
  if (mismatch !== null) return mismatch
  return cellsOf(hits, rule, calls)
})
