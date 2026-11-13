/**
 * The NEUTRAL cut of one capture: what the base's case generator
 * (`base-generator.ts`) makes of a flows document when no deployment states a
 * rule. The flows document is the one `*.flows.json` the capture's source
 * directory holds. The system under test is the address that took the
 * capture's earliest dialog-creating INVITE; its legs are joined into calls by
 * the Call-ID derivation the caller names (none by default, `derivation.ts`),
 * and each call at its boundary is cut and assembled under
 * the `@sip/pipeline` neutral policy (`CaseAssembler`), which states nothing it
 * did not observe; each call is named `<capture stem>-<n>` in cut order, so a
 * document cut again names its calls as before. Each case is
 * written as `cases/<id>/<id>.v3.json` with its `resources/`, and every
 * refusal as an `excluded.json` record beside them (`cut-contract.ts`).
 */
import { type Pivot as PivotDoc, Flows, Pivot } from "@sip/contracts"
import { CaseAssembler, Cut, FlowsFile, Parts, Plan, Profile, Sut } from "@sip/pipeline"
import { type CallIdDerivation, neverDerives } from "@sip/pipeline/derivation"
import { type ExclusionRecord, exclusionRecordOf } from "@sip/pipeline/exclusion-record"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Schema from "effect/Schema"
import * as path from "node:path"
import { documentFile, linkDocument } from "./case-dir.js"
import type { CaptureWork } from "./cut-contract.js"
import { EXCLUDED } from "./excluded.js"

/** A plan classifying no number: the neutral cut reads no numbering plan. */
const NO_PLAN = new Plan.Plan({ default_cc: "", countries: [], max_composed_prefix: 0, fake_prefix: "" })

/** Every IPv4 address: the system under test may sit anywhere. */
const ANY_ADDRESS = new Profile.SutProfile(["0.0.0.0/0"])

/** What the cut made of one capture: the ids of the cases it wrote and the refusals it recorded. */
export interface NeutralCut {
  readonly cases: ReadonlyArray<string>
  readonly refused: ReadonlyArray<ExclusionRecord>
  /** The system under test the cut decided, and why. */
  readonly sut: string
}

/** The name of `capture`'s `n`th call: its file name's stem and the number. */
const nameOf = (capture: string, n: number): string => `${capture.replace(/\.(pcapng|pcap)(\.gz)?$/, "")}-${n}`

/** One case written into `caseDir`: its document and its resources. */
const writeCase = (caseDir: string, id: string, pivot: PivotDoc.PivotV3, resources: ReadonlyArray<{ relPath: string; bytes: Uint8Array }>) =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    yield* fs.makeDirectory(path.join(caseDir, "resources"), { recursive: true })
    yield* fs.writeFileString(path.join(caseDir, documentFile(id)), Pivot.emitPivot(pivot))
    for (const resource of resources) yield* fs.writeFile(path.join(caseDir, resource.relPath), resource.bytes)
    yield* linkDocument(caseDir, id)
  })

/** The capture's flows document cannot be read; `reason` says why. */
export class Unreadable extends Schema.TaggedError<Unreadable>()("NeutralCut.Unreadable", { reason: Schema.String }) {
  override get message(): string {
    return this.reason
  }
}

/** The one flows document (`*.flows.json`) `work`'s source directory holds, read. */
const flowsOf = (work: CaptureWork) =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const named = (yield* fs.readDirectory(work.srcDir)).filter((file) => file.endsWith(".flows.json"))
    if (named.length !== 1) {
      return yield* new Unreadable({ reason: `${work.srcDir} holds ${named.length} flows documents, not one` })
    }
    const flows = yield* Effect.try({
      try: () => FlowsFile.readFlowsFile(path.join(work.srcDir, named[0]!)),
      catch: (error) => new Unreadable({ reason: `${named[0]}: ${String(error)}` })
    })
    if (flows.schema !== Flows.EMIT_SCHEMA_VERSION) {
      return yield* new Unreadable({ reason: `${named[0]}: flows schema ${flows.schema}, expected ${Flows.EMIT_SCHEMA_VERSION}` })
    }
    return flows
  })

/** The flows document of `work`, cut and written into its `outDir`, wiped first. */
export const cutCapture = Effect.fn("NeutralCut.cutCapture")(function*(
  work: CaptureWork,
  derives: CallIdDerivation = neverDerives
) {
  const fs = yield* FileSystem.FileSystem
  const assembler = yield* CaseAssembler.Service
  const flows = yield* flowsOf(work)
  const decision = Sut.decideSut(flows, ANY_ADDRESS, work.capture)
  let n = 0
  const cut = Cut.cutCalls(flows, decision.sut, work.capture, derives, () => nameOf(work.capture, ++n))
  const decided = yield* assembler.cases({
    flows,
    capture: work.capture,
    specs: cut.calls.map((call) => ({
      id: call.id,
      uac: call.uac,
      uas: [...call.uas],
      defects: [],
      cutLegs: call.legs,
      ...(call.ingressOrdinal === undefined ? {} : { ingressOrdinal: call.ingressOrdinal }),
      ...(call.ingress === undefined ? {} : { ingress: call.ingress }),
      sutNote: decision.evidence
    })),
    sut: decision.sut,
    plan: NO_PLAN,
    parts: Parts.index(flows)
  })
  yield* fs.remove(work.outDir, { recursive: true, force: true })
  yield* fs.makeDirectory(path.join(work.outDir, "cases"), { recursive: true })
  const refused = [
    ...cut.refused.map((refusal) =>
      exclusionRecordOf(refusal, { legs: refusal.legs ?? [], callIds: refusal.callIds ?? [], sut: decision.sut.addresses })
    ),
    ...decided.outcomes.flatMap((outcome) =>
      outcome.refused.map((refusal) =>
        exclusionRecordOf(refusal, { legs: outcome.legs, callIds: outcome.callIds, sut: decision.sut.addresses })
      )
    )
  ]
  const cases: Array<string> = []
  for (const outcome of decided.outcomes) {
    if (outcome.built === undefined) continue
    yield* writeCase(path.join(work.outDir, "cases", outcome.spec.id), outcome.spec.id, outcome.built.pivot, outcome.built.resources)
    cases.push(outcome.spec.id)
  }
  if (refused.length > 0) {
    yield* fs.writeFileString(path.join(work.outDir, EXCLUDED), `${JSON.stringify(refused, null, 2)}\n`)
  }
  return { cases, refused, sut: decision.evidence } satisfies NeutralCut
})
