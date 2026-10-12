/**
 * The row vocabulary: what a run and a case result ARE, independent of SQL.
 *
 * `status` is the run's own outcome, not a verdict roll-up: `done` means the
 * driver produced a rollup (cases then say pass, fail or skip), `no-cases` that
 * the cut refused every case, `error` that the pipeline broke before a rollup,
 * `interrupted` that the job was stopped with cells still to run — the rollup
 * holds what landed. Only a `done` run with at least one case that RAN and no
 * failure is green; an interrupted run is a case's last run for what it
 * landed and never a capture's standing.
 */
import { MainError as MainErrors } from "@sip/contracts"
import * as Schema from "effect/Schema"

export const RunStatus = Schema.Literals(["done", "no-cases", "error", "interrupted"])
export type RunStatus = typeof RunStatus.Type

/**
 * One repository whose code produced a row: its name as the deployment calls
 * it, its commit, whether its tree was dirty, and the content FINGERPRINT of
 * the tree. A sha is an identity only on a clean tree — two mid-edit sweeps
 * share one — so the fingerprint is what says two runs ran the same code. Null
 * means it could not be taken.
 */
export const Component = Schema.Struct({
  name: Schema.String,
  sha: Schema.NullOr(Schema.String),
  dirty: Schema.NullOr(Schema.Boolean),
  tree: Schema.NullOr(Schema.String)
})
export interface Component extends Schema.Schema.Type<typeof Component> {}

/**
 * Which code produced the row: one entry per repository the deployment builds
 * from, in the order it names them. Empty is unknown provenance — a row whose
 * code was never taken — which compares as unknown against anything.
 */
export const Provenance = Schema.Array(Component)
export type Provenance = ReadonlyArray<Component>

export const UNKNOWN_PROVENANCE: Provenance = []

/** Whether any repository of the provenance was dirty; unknown dirtiness is not dirty. */
export const isDirty = (code: Provenance): boolean => code.some((component) => component.dirty === true)

/** The component of that name, where the provenance names it. */
export const componentOf = (code: Provenance, name: string): Component | undefined =>
  code.find((component) => component.name === name)

/** The provenance on one line: `<name> <first 10 of field>` per component, `unknown` where it names none. */
export const codeLine = (code: Provenance, field: "sha" | "tree"): string =>
  code.length === 0
    ? "unknown"
    : code.map((component) => `${component.name} ${component[field]?.slice(0, 10) ?? "unknown"}`).join(", ")

export const MainErrorKind = MainErrors.MainErrorKind
export type MainErrorKind = MainErrors.MainErrorKind

/** What a failing cell is said to have failed on (`@sip/contracts` `main-error.ts`). */
export const MainError = MainErrors.MainError
export type MainError = MainErrors.MainError

/** One case's outcome inside a run, as the bundle stated it. */
export const CaseResult = Schema.Struct({
  caseId: Schema.String,
  passed: Schema.Boolean,
  /** `verdict.json`'s own `status` (`ok` / `failed`), or `absent` when it wrote none. */
  verdict: Schema.String,
  /**
   * The lane verdict the document blocked this case's lane with
   * (`blocked:<reason>`), verbatim; null = the case ran. A skipped case is
   * neither a pass nor a failure: nothing was asked of the SUT.
   */
  skipped: Schema.NullOr(Schema.String),
  failedStep: Schema.NullOr(Schema.String),
  /**
   * Gating findings of the lane's post-run RFC audit (`rfc.json`); null where
   * the cell was not audited, which is not a zero.
   */
  rfcGating: Schema.NullOr(Schema.Number),
  /** The body rows no rule accepts, whatever the main error names; absent reads as 0. */
  bodyFailing: Schema.optionalKey(Schema.Number),
  /** What the cell failed on; null where it passed, was skipped, or was recorded unmeasured. */
  mainError: Schema.NullOr(MainError),
  classification: Schema.Unknown,
  /** The v3 case document this case ran, for content-hash storage; null = not captured. */
  document: Schema.Unknown
})
export interface CaseResult extends Schema.Schema.Type<typeof CaseResult> {}

/**
 * The delta classification of a run's cases, summed. It rules on the
 * confrontation records ONLY — what the replay's wire differed from the
 * capture by, and whether a rule already accounts for it. It is NOT the run's
 * outcome: a run that aborted mid-script can carry nothing but accepted
 * deltas, so `Standing.failed` is what says whether the replay held.
 */
export const Deltas = Schema.Struct({
  /** Cases that carried a classification at all; 0 = the counts say nothing. */
  cases: Schema.Number,
  accepted: Schema.Number,
  knownBug: Schema.Number,
  unlisted: Schema.Number,
  unknown: Schema.Number,
  compared: Schema.Number,
  unreferenced: Schema.Number,
  records: Schema.Number
})
export interface Deltas extends Schema.Schema.Type<typeof Deltas> {}

export const NO_DELTAS: Deltas = {
  cases: 0,
  accepted: 0,
  knownBug: 0,
  unlisted: 0,
  unknown: 0,
  compared: 0,
  unreferenced: 0,
  records: 0
}

/**
 * One case's outcome inside the standing run, kept AS A CASE. A capture that
 * cut several calls is several independent questions: rolling their steps into
 * one list of the run's failures loses which call died where, which is the only
 * thing a reader can act on.
 */
export const CaseOutcome = Schema.Struct({
  caseId: Schema.String,
  passed: Schema.Boolean,
  /** The step this case died at; null where it passed or named none. */
  failedStep: Schema.NullOr(Schema.String),
  /** The lane verdict this case was blocked on; null where it ran. */
  skipped: Schema.NullOr(Schema.String)
})
export interface CaseOutcome extends Schema.Schema.Type<typeof CaseOutcome> {}

export const Run = Schema.Struct({
  id: Schema.Number,
  capture: Schema.String,
  lane: Schema.String,
  mode: Schema.String,
  status: RunStatus,
  bundlePath: Schema.String,
  detail: Schema.NullOr(Schema.String),
  startedAt: Schema.String,
  finishedAt: Schema.NullOr(Schema.String)
})
export interface Run extends Schema.Schema.Type<typeof Run> {}

/**
 * The case set a run names: `full` is every case the pinned prep cut for the
 * capture, `cases` an explicit subset, `timeline` the capture whole on its
 * captured timing. Any of them is a run of the cases it holds; a
 * `full` one also states the capture's case set.
 */
export const SetKind = Schema.Literals(["full", "cases", "timeline"])
export type SetKind = Schema.Schema.Type<typeof SetKind>

/** A case's CURRENT run: its latest `@current` run of any set kind whose results hold it. */
export const CaseRun = Schema.Struct({
  caseId: Schema.String,
  runId: Schema.Number,
  setKind: SetKind,
  passed: Schema.Boolean,
  /** The lane verdict the case was blocked on in that run; null where it ran. */
  skipped: Schema.NullOr(Schema.String),
  bundlePath: Schema.String
})
export interface CaseRun extends Schema.Schema.Type<typeof CaseRun> {}

/**
 * A (capture, lane)'s current standing: its latest `@current`
 * run, its latest green full `@current` run, and its cases each at its current
 * run. The counts and steps are the current cases'.
 */
export const Standing = Schema.Struct({
  capture: Schema.String,
  lane: Schema.String,
  runId: Schema.Number,
  status: RunStatus,
  /** The prep the standing run read its cases as of; null for a run pinned to none. */
  prepId: Schema.NullOr(Schema.Number),
  bundlePath: Schema.String,
  finishedAt: Schema.NullOr(Schema.String),
  cases: Schema.Number,
  failed: Schema.Number,
  /** Cases the driver skipped on a blocked lane; they count in neither `failed` nor green. */
  skipped: Schema.Number,
  caseIds: Schema.Array(Schema.String),
  /** Steps the failing cases died at, in case order; empty where none named one. */
  failedSteps: Schema.Array(Schema.String),
  /** The distinct lane verdicts the skipped cases were blocked by. */
  skipReasons: Schema.Array(Schema.String),
  /** Every current case, as itself: which call, and how IT ended at its current run. */
  caseResults: Schema.Array(CaseOutcome),
  /** Gating RFC-audit findings over the current cases; 0 where none was audited. */
  rfcGating: Schema.Number,
  /** The current cases' confrontation classification, summed. Not the run's outcome. */
  deltas: Deltas,
  /** The last full `@current` run of this (capture, lane) that was green; null = never green. */
  lastOkRunId: Schema.NullOr(Schema.Number),
  lastOkBundlePath: Schema.NullOr(Schema.String),
  /** Each case's current run, in case id order. */
  caseRuns: Schema.Array(CaseRun)
})
export interface Standing extends Schema.Schema.Type<typeof Standing> {}

/** The three counts a standing's ruling turns on. */
export interface Outcome {
  readonly status: RunStatus
  readonly cases: number
  readonly failed: number
  readonly skipped: number
}

/** Green: the driver finished, at least one case RAN, and none of them failed. */
export const isGreen = (standing: Outcome): boolean =>
  standing.status === "done" && standing.cases - standing.skipped > 0 && standing.failed === 0

/**
 * Skipped: the driver finished and every case it cut was blocked on this lane.
 * Neither green nor failing — the lane was never asked the question.
 */
export const isSkipped = (standing: Outcome): boolean =>
  standing.status === "done" && standing.cases > 0 && standing.skipped === standing.cases


/** A file a case directory holds beside its document, at a case-relative path. */
export const CaseAsset = Schema.Struct({
  path: Schema.String,
  bytes: Schema.Uint8Array
})
export interface CaseAsset extends Schema.Schema.Type<typeof CaseAsset> {}

/** One replay-ledger row, keyed by the number the case dials to reach it. */
export const LedgerRow = Schema.Struct({
  number: Schema.String,
  document: Schema.Unknown
})
export interface LedgerRow extends Schema.Schema.Type<typeof LedgerRow> {}

/**
 * Everything ONE case rebuilds from, by content hash. It carries no case id:
 * the id lives inside the document (`case.id`), so two identical cases are one
 * row and the materialiser reads the name off what it stored.
 */
export const CaseContent = Schema.Struct({
  document: Schema.Unknown,
  assets: Schema.Array(CaseAsset),
  ledger: Schema.Array(LedgerRow)
})
export interface CaseContent extends Schema.Schema.Type<typeof CaseContent> {}

/**
 * What storing one case did: its content hash, and whether the CASE was new to
 * the store. `fresh` false is the agreement a re-cut wants — the store already
 * held that document, those assets and those ledger rows — so a cut reports the
 * two counts rather than assuming either.
 */
export const RecordedCase = Schema.Struct({
  caseId: Schema.String,
  caseHash: Schema.String,
  fresh: Schema.Boolean
})
export interface RecordedCase extends Schema.Schema.Type<typeof RecordedCase> {}

/** What one capture's record wrote, case by case. */
export const Recorded = Schema.Struct({
  cases: Schema.Array(RecordedCase)
})
export interface Recorded extends Schema.Schema.Type<typeof Recorded> {}

/**
 * One case a cut produced, whole: the v3 document, its assets and its ledger
 * rows. All three travel together because all three are hashed together — the
 * rows are a reading of the CAPTURE, not a function of the document, so a name
 * blind to them would call two different cases one case.
 */
export const CutCaseRequest = Schema.Struct({
  caseId: Schema.String,
  /** The capture's own name for the call this case was cut from (`call-key.ts`). */
  callKey: Schema.String,
  document: Schema.Unknown,
  assets: Schema.Array(CaseAsset),
  ledger: Schema.Array(LedgerRow),
  /** The endpoint egress leaves by, where the cut named one other than the default. */
  egressEndpoint: Schema.optionalKey(Schema.NullOr(Schema.String))
})
export interface CutCaseRequest extends Schema.Schema.Type<typeof CutCaseRequest> {}

/** One piece of a flows document, in document order. */
export interface FlowsPart {
  readonly kind: "head" | "leg" | "tail"
  readonly bytes: Uint8Array
}

/**
 * A flows document offered to the store as the pieces it is kept in: the
 * envelope's head, each leg, then the tail. It is called once per pass, so a
 * document of a gigabyte is read again rather than held.
 */
export type FlowsSource = () => Iterable<FlowsPart>

/** A document the caller holds whole: the envelope alone, with no leg between its halves. */
export const wholeFlows = (bytes: Uint8Array): FlowsSource => () => [
  { kind: "head", bytes },
  { kind: "tail", bytes: new Uint8Array(0) }
]

/** A stored document's envelope, and how many legs stand between its halves. */
export interface FlowsEnvelope {
  readonly head: Uint8Array
  readonly tail: Uint8Array
  readonly legs: number
}

/**
 * One call the cut REFUSED to make a case of: the case id it would have had,
 * every roster refusal id it was refused under (the first leads), the
 * Call-IDs it rests on, and the one-line evidence. A writer may hand one
 * record per refusing verdict; the store keeps one row per call.
 */
export const Refusal = Schema.Struct({
  caseId: Schema.String,
  reasons: Schema.Array(Schema.String),
  callIds: Schema.Array(Schema.String),
  detail: Schema.String,
  /** The call the refusal names, where the cut stated it: what freezes its id. */
  callKey: Schema.optional(Schema.String)
})
export interface Refusal extends Schema.Schema.Type<typeof Refusal> {}

/**
 * ONE capture's cut, as the writer hands it over. A cut that BROKE carries
 * `broke` and no cases; a cut that refused every case carries neither. The
 * refusals are the cut's own record of every call it made no case of; a cut
 * that refused nothing passes none. Hand built and never decoded, so the flows
 * document travels as a source rather than as bytes nothing could hold.
 */
export interface CaptureCutRequest {
  readonly prepId: number
  readonly capture: string
  /** The enriched flows document every case of this capture confronts; null where the cut got none. */
  readonly flows: FlowsSource | null
  readonly broke?: string | null
  /** The cut CODE this reading came from, as a content hash; null where the writer named none. */
  readonly cutFingerprint?: string | null
  readonly cases: ReadonlyArray<CutCaseRequest>
  /** Every call this cut refused to make a case of; empty where it refused none. */
  readonly refusals: ReadonlyArray<Refusal>
}

/** One case of a standing cut: the content to materialise, and how to run it. */
export const CutCase = Schema.Struct({
  caseId: Schema.String,
  /** The capture's own name for the call, which a request cell names it by. */
  callKey: Schema.String,
  caseHash: Schema.String,
  egressEndpoint: Schema.NullOr(Schema.String)
})
export interface CutCase extends Schema.Schema.Type<typeof CutCase> {}

/**
 * Why a call key resolves to no case of a cut: `refused` where the cut's
 * refusals name the key's Call-ID, with their reasons; `not-in-this-cut` where
 * nothing of the cut names it, reasons empty.
 */
export interface Gone {
  readonly gone: "refused" | "not-in-this-cut"
  readonly reasons: ReadonlyArray<string>
}

/** What one call key resolves to against one cut: the case it names, or why it names none. */
export type Resolved = CutCase | Gone

export const isGone = (resolved: Resolved): resolved is Gone => "gone" in resolved

/**
 * What one prep made of one capture. `broke` non-null is a cut that FAILED.
 * `broke` null with an empty `cases` is the `no-cases` reading — the cut
 * refused every case, which is an answer rather than a failure, and the queue
 * rules on that difference.
 */
export const CaptureCut = Schema.Struct({
  prepId: Schema.Number,
  capture: Schema.String,
  /** The flows document the cases confront; null where the cut never got one. */
  flowsHash: Schema.NullOr(Schema.String),
  broke: Schema.NullOr(Schema.String),
  cutAt: Schema.String,
  cases: Schema.Array(CutCase)
})
export interface CaptureCut extends Schema.Schema.Type<typeof CaptureCut> {}

/**
 * What a capture STANDS cut as, without its cases: the newest reading of it,
 * and the two facts that say whether that reading is still current — the cut
 * code it came from, and when it was taken.
 */
export const CaptureCutState = Schema.Struct({
  prepId: Schema.Number,
  flowsHash: Schema.NullOr(Schema.String),
  cutFingerprint: Schema.NullOr(Schema.String),
  broke: Schema.NullOr(Schema.String),
  cutAt: Schema.String
})
export interface CaptureCutState extends Schema.Schema.Type<typeof CaptureCutState> {}

/**
 * One capture the replay POPULATION holds: where its pcap was read from, when
 * it joined, and when the artifact store last extracted it.
 */
export const PopulationMember = Schema.Struct({
  capture: Schema.String,
  /** The pcap this capture was extracted from, as the host named it; null for a seeded row. */
  sourcePath: Schema.NullOr(Schema.String),
  addedAt: Schema.String,
  ingestedAt: Schema.NullOr(Schema.String),
  /** The extractor CODE that took the standing artifacts (a content hash); null before any ingest recorded one. */
  extractorHash: Schema.NullOr(Schema.String),
  /** Why the last ingest refused this capture; null where it extracted. */
  ingestFailed: Schema.NullOr(Schema.String)
})
export interface PopulationMember extends Schema.Schema.Type<typeof PopulationMember> {}
