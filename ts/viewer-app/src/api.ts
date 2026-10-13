/** The server client. One fetch helper, one hook, no state library. */
import type { CompareReport, RunReport } from "@sip/contracts"
import type { HttpExchanges } from "@sip/viewer-core/http"
import { useEffect, useState } from "react"

/**
 * The generator's refusal to cut a case, served verbatim. An entry carrying one
 * has NO scenario: there is nothing to draw, to compare or to run, and the
 * refusal is what the page shows instead.
 */
export interface CaseExclusion {
  readonly path: string
  readonly document: unknown
}

export interface CaseSummary {
  readonly id: string
  readonly rootId: string
  readonly caseId: string
  /** The document's `case.id`: the key a run bundle names its case by. */
  readonly caseKey: string
  readonly title: string
  readonly family: string
  readonly origin: string
  readonly variant: string
  readonly lanes: Readonly<Record<string, string>>
  readonly path: string
  /** Empty for an excluded entry. */
  readonly variants: readonly string[]
  readonly excluded?: CaseExclusion | undefined
}

export interface CaseDetail extends CaseSummary {
  readonly document: unknown
  readonly resources: readonly string[]
  /**
   * What the lane's HTTP mock answers for the case, as the server reads the
   * deployment's record of it. `null` where the case has none.
   */
  readonly ledger: MockAnswers | null
}

/** One answer a lane's HTTP mock serves: its label, the request it matches on one line, the answer verbatim. */
export interface MockAnswer {
  readonly label: string
  readonly match: string
  readonly answer: unknown
}

/** What a lane's HTTP mock answers for one case: the target it answers for, what the capture observed, the answers. */
export interface MockAnswers {
  readonly target: string | null
  readonly observed: string | null
  readonly answers: ReadonlyArray<MockAnswer>
}

/** One window of a flows document's legs, at the indices the document gives them. */
export interface FlowsLegs {
  readonly from: number
  readonly legs: readonly unknown[]
}

/**
 * The capture a case was cut from: its document with the legs left out, and the
 * window the pane opens on. The rest of the legs come per window, off a route
 * of their own — a capture of thousands of calls has no document that fits one
 * response.
 */
export interface CaseSource {
  readonly available: boolean
  readonly capture?: string | null
  readonly callGroups: readonly number[]
  readonly path?: string | null
  readonly document: unknown
  /** Legs the document holds, however few this answer carries. */
  readonly legs: number
  readonly window: FlowsLegs
  /** The case's legs past that window, one window per run of them: the pane opens on them too. */
  readonly cited: readonly FlowsLegs[]
  readonly detail: string
}

export interface RunSummary {
  readonly id: string
  readonly rootId: string
  readonly runId: string
  /** The `case.id` of the bundle's `pivot.json`: the case this run replayed. */
  readonly caseKey: string
  readonly path: string
  readonly legs: readonly string[]
  readonly verdict: unknown
}

export interface RunDetail extends RunSummary {
  readonly pivot: unknown
  readonly timing: unknown
  readonly runConfig: unknown
  readonly recording: Readonly<Record<string, string>>
}

export interface TriageTicket {
  /** The ticket the pending record waits on, as the deployment's tracker names it. */
  readonly ref: string
  /** open | closed | wontfix | missing. */
  readonly state: string
}

/** What a call failed on: the one line a queue row is picked on. */
export interface TriageMainError {
  /** rfc = a charged RFC finding; harness-fault = a scripted party broke a rule the document does not state; died = the replay stopped waiting; failure = any other verdict failure; bad-body = a body no rule accepts; refused = the cut made no case. */
  readonly kind: "rfc" | "harness-fault" | "died" | "failure" | "bad-body" | "refused"
  /** The RFC rule id, the failure tag, or the refusal reason; null for a died call. */
  readonly rule: string | null
  /** The citation the finding carries (`RFC 3581 §4`); null where it cites none. */
  readonly rfc: string | null
  readonly step: string | null
  /** What a died call waited for, as `BYE` / `200 to INVITE` / `200`. */
  readonly expected: string | null
  readonly detail: string
}

/** One case's outcome inside a run: which call, and how it ended. */
export interface CaseOutcome {
  readonly caseId: string
  readonly passed: boolean
  /** The step this case died at; null where it passed or named none. */
  readonly failedStep: string | null
  /** The lane verdict this case was blocked on; null where it ran. */
  readonly skipped: string | null
}

/**
 * One CALL of the lane's queue: a case of the standing run of its capture, at
 * the case's own last run, or a call the standing cut refused to make a case
 * of. The document's facts are joined for the served page only.
 */
export interface TriageCallRow {
  readonly capture: string
  readonly caseId: string
  readonly kind: "case" | "refused"
  /** The call key a request cell names the call by; null where no cut names the case. */
  readonly callKey: string | null
  /** The case's own last run; null for a refused call. */
  readonly runId: number | null
  /** full | cases | timeline — the set that run named; null for a refused call. */
  readonly setKind: "full" | "cases" | "timeline" | null
  /** The standing cut a refused call belongs to; null for a case. */
  readonly prepId: number | null
  readonly bundlePath: string | null
  readonly documentHash: string | null
  readonly passed: boolean
  /** The lane verdict the case was blocked on; null where it ran. */
  readonly skipped: string | null
  readonly failedStep: string | null
  readonly rfcGating: number | null
  /** Every reason a refused call was refused under, the first being `mainError.rule`; [] for a case. */
  readonly reasons: readonly string[]
  /** The features of the document the case ran (reroute, 18x mode, transfer, MRF …); [] for a refused call. */
  readonly features: readonly string[]
  /** Null where the call passed, was skipped, or was never measured. */
  readonly mainError: TriageMainError | null
  /** The body rows no rule accepts, whatever `mainError` names; absent reads as 0. */
  readonly bodyFailing?: number
  /** When the run finished, or when the refusing cut was taken. */
  readonly finishedAt: string | null
  readonly family: string | null
  /** The number the first attempt dialled. */
  readonly dialled: string | null
  /** The final status of the first call's last attempt, as text. */
  readonly outcome: string | null
  readonly legs: number | null
  readonly attempts: number | null
  /** The run's bundle survives on disk; false = cleaned, row intact. */
  readonly bundle: boolean
}

export interface TriageFacetCount {
  readonly value: string
  readonly count: number
}

/**
 * The chip counts, over the rows the scope and the capture keep — never
 * narrowed by the chips themselves, so a count answers "in this scope and
 * capture, how many calls died on X" whatever chip is active.
 */
export interface TriageFacets {
  /** The charged RFC rules, with the citation they carry. */
  readonly rules: readonly (TriageFacetCount & { readonly rfc: string | null })[]
  /** Every other failure tag of a case, the interpreter's refusal included. */
  readonly failures: readonly TriageFacetCount[]
  readonly expected: readonly TriageFacetCount[]
  /** A call refused under several reasons counts once under each. */
  readonly refusals: readonly TriageFacetCount[]
  /** The row kinds — refused, failed, passed, skipped — in that order; an excluded one counts what it hides. */
  readonly outcomes: readonly TriageFacetCount[]
  /** The features, whatever the outcome; a call counts once under each it carries. */
  readonly features: readonly TriageFacetCount[]
  /** Distinct captures among the rows the scope, the capture filter and the excludes keep. */
  readonly captures: number
}

/**
 * A case's OWN last run: the most recent run of any set kind whose results
 * hold it. `setKind` is `full` (the capture's whole set, what the standing
 * reads) or `cases` (this call replayed alone, which moved no standing).
 */
export interface CaseRun {
  readonly caseId: string
  readonly runId: number
  readonly setKind: "full" | "cases" | "timeline"
  readonly passed: boolean
  /** The lane verdict the case was blocked on in that run; null where it ran. */
  readonly skipped: string | null
  readonly bundlePath: string
}

/** A lane the run database holds runs on, and how much of it. */
export interface TriageLane {
  readonly lane: string
  readonly runs: number
  readonly captures: number
}

/** One run of one call, on any lane: how the call ended in it. */
export interface TriageCallHistoryEntry {
  readonly lane: string
  readonly runId: number
  readonly setKind: string
  readonly jobId: number | null
  readonly finishedAt: string | null
  readonly passed: boolean
  /** The lane verdict the case was blocked on; null where it ran. */
  readonly skipped: string | null
  readonly failedStep: string | null
  readonly errorRule: string | null
  readonly errorExpected: string | null
  /** The features of the document the run ran. */
  readonly features: readonly string[]
}

/** A call's latest run on each lane a request can name, and its recent runs across every lane. */
export interface TriageCallHistory {
  /** One per lane that ran the call, in `GET /api/lanes` order. */
  readonly latest: readonly TriageCallHistoryEntry[]
  /** Newest first, whatever the lane. */
  readonly recent: readonly TriageCallHistoryEntry[]
}

/** What the queue was asked for. Absent fields take the server's defaults. */
export interface TriageQuery {
  readonly lane?: string | undefined
  /** `red` (the default), `all`, `passed`, `failed`, `skipped` or `refused`. */
  readonly scope?: string | undefined
  /** Capture-name substring. */
  readonly q?: string | undefined
  /** Exact main-error rule of a case row: an RFC rule id or a failure tag alike. */
  readonly rule?: string | undefined
  /** Exact message a died call waited for. */
  readonly expected?: string | undefined
  /** Exact refusal reason of a refused call; any of its reasons answers. */
  readonly refusal?: string | undefined
  /** Exact row kind: refused | failed | passed | skipped. */
  readonly outcome?: string | undefined
  /** One feature the call's document carries. */
  readonly feature?: string | undefined
  /** The values hidden per group; the key repeats per value on the wire. */
  readonly "not-rule"?: ReadonlyArray<string> | undefined
  readonly "not-expected"?: ReadonlyArray<string> | undefined
  readonly "not-refusal"?: ReadonlyArray<string> | undefined
  readonly "not-outcome"?: ReadonlyArray<string> | undefined
  readonly "not-feature"?: ReadonlyArray<string> | undefined
  readonly limit?: number | undefined
}

/** One record of a delta line, as the call it came from shows it. */
export interface DeltaSample {
  readonly capture: string
  readonly caseId: string
  readonly session: string
  readonly cell: string
  readonly step: string
  readonly captured: readonly string[]
  readonly replayed: readonly string[]
}

/** Every confrontation record of a queue slice sharing one signature, class and rule. */
export interface DeltaLine {
  readonly kind: string
  readonly signature: string
  readonly klass: string
  readonly rule: string
  readonly rows: number
  readonly cells: number
  readonly samples: readonly DeltaSample[]
}

/** The delta sheet of a queue slice: every kept call's confrontation records, folded. */
export interface DeltaSheet {
  readonly available: boolean
  readonly detail: string
  readonly lane: string
  readonly scope: string
  readonly matched: number
  readonly read: number
  readonly unread: number
  readonly undecodable: number
  readonly elapsedMs: number
  readonly lines: readonly DeltaLine[]
}

export interface TriageQueue {
  readonly available: boolean
  readonly detail: string
  /** The lane this answer reads. */
  readonly lane?: string
  readonly lanes: readonly TriageLane[]
  readonly scope: string
  /** Rows the filter kept, of which `rows` carries at most `limit`. */
  readonly matched: number
  readonly limit: number
  readonly facets: TriageFacets
  readonly rows: readonly TriageCallRow[]
}

/**
 * One replay of one call as the run index LISTS it: how it ENDED, and nothing
 * more. A capture gains a cell per sweep and the page reads one of them, so the
 * verdict and the evidence are fetched per cell — `api.triageCell`.
 */
export interface TriageStudyCell {
  readonly name: string
  readonly dir: string
  /** The lane this cell ran on, as its directory name states it. */
  readonly lane?: string
  /** When this call's evidence was last written. */
  readonly writtenAt?: string
  /** `verdict.json`'s own status word; absent where the cell wrote none. */
  readonly status?: string
  /** Whether the cell passed, as the run's index states it; absent where the run wrote no index naming it. */
  readonly passed?: boolean
  /** The step the verdict says the replay died at. */
  readonly failedStep?: string
  /** The lane verdict this cell was SKIPPED on; absent where the cell ran. */
  readonly skipped?: string
  /** The interpreter refused this case: a refusal where a bundle should be. */
  readonly refused: boolean
}

/** The evidence of ONE cell: everything the run index leaves behind. */
export interface TriageStudyCellDetail {
  readonly name: string
  readonly dir: string
  readonly legs: readonly string[]
  readonly verdict: unknown
  readonly classification: unknown
  /** The post-run RFC audit (`rfc.json`): what the traffic broke, gating or not. */
  readonly rfc: unknown
  readonly skipped: unknown
  readonly error?: string
  /** What the interpreter wrote on stderr, where the cell ran and did not pass. */
  readonly stderr?: string
  readonly pivot: unknown
  readonly timing: unknown
  readonly runConfig: unknown
  readonly runSpec: unknown
  readonly recording: Readonly<Record<string, string>>
  /** The exchanges this cell's SUT had with an HTTP service, as the server read the lane's record; null where it kept none. */
  readonly http: HttpExchanges | null
  /** The calls the SUT reported terminated, as the cell kept them; null where it kept none. */
  readonly terminated: unknown
  readonly confrontation: readonly unknown[]
  readonly confrontationProblem?: string
}

export interface TriageStudySession {
  readonly name: string
  readonly dir: string
  /** When the run directory was last written: how fresh this run is. */
  readonly writtenAt?: string
  readonly campaign: unknown
  readonly cells: readonly TriageStudyCell[]
  /** What the configured hooks recorded around this run; [] where none did. */
  readonly hooks: readonly TriageStudyHook[]
}

/** One hook log kept beside a run: `hooks/<name>.log`, verbatim. */
export interface TriageStudyHook {
  readonly name: string
  readonly text: string
}

export interface TriageStudyCase {
  readonly id: string
  readonly dir: string
  readonly document: unknown
  /** What the lane's HTTP mock answers for the case; null where it has no record. */
  readonly ledger: MockAnswers | null
}

/** The run database's view of a capture: latest run, and the last green one. */
/**
 * What the cut store says a capture stands cut as. It is the reading the cases
 * and the flows document below come from, which need not be the one a given
 * lane's run replayed.
 */
export interface TriageCut {
  readonly prepId: number
  readonly cutAt: string
  /** The cut failed with this reason; null for a cut that produced a reading. */
  readonly broke: string | null
  readonly cases: number
}

export interface TriageStanding {
  readonly lane: string
  readonly runId: number
  /** The prep the standing run read its cases as of; null for a run pinned to none. */
  readonly prepId: number | null
  readonly bundlePath: string
  readonly cases: number
  readonly failed: number
  readonly failedSteps: readonly string[]
  readonly caseResults: readonly CaseOutcome[]
  readonly rfcGating: number
  readonly finishedAt: string | null
  readonly lastOkRunId: number | null
  readonly lastOkBundlePath: string | null
  /** Each case's own last run, which may be a `cases` run the standing ignores. */
  readonly caseRuns: readonly CaseRun[]
}

/** Where a capture's flows document is, and how many legs it holds. */
export interface FlowsPlace {
  readonly path: string
  readonly legs: number
}

/** That document with its legs left out: what a ladder reads once. */
export interface FlowsEnvelope extends FlowsPlace {
  readonly document: unknown
}



/** The standing cut's refusal of one call: every reason, the Call-IDs, the cut's own line. */
export interface TriageRefusal {
  readonly caseId: string
  readonly reasons: readonly string[]
  readonly callIds: readonly string[]
  readonly detail: string
}

export interface TriageStudy {
  readonly capture: string
  /** The one call this study is of; absent for the capture whole. */
  readonly case?: string
  /** The standing cut's refusal of that call, where it is one the cut made no case of. */
  readonly refusal?: TriageRefusal
  readonly disposition: string
  readonly record: unknown
  readonly ticket?: TriageTicket
  readonly standing?: TriageStanding
  /** The standing cut; absent where the store holds no cut of this capture. */
  readonly cut?: TriageCut
  readonly corpusDir?: string
  /** Where the flows document is and how many legs it holds; its legs come per window. */
  readonly flows?: FlowsPlace
  readonly auto: unknown
  /** The generator's refusal to cut, where it wrote one: a `no-cases` capture's evidence. */
  readonly exclusions?: { readonly path: string; readonly document: unknown }
  readonly cases: readonly TriageStudyCase[]
  readonly sessionDir?: string
  readonly sessions: readonly TriageStudySession[]
  /** Where the absolute paths above are rooted: what makes a path repo-relative. */
  readonly repoRoot: string
}

/** What a drop of one capture started, and where that capture will be readable. */
export interface CaptureIngestStart {
  /** The capture name the campaign will use: the study view's own key. */
  readonly capture: string
  readonly started: boolean
  readonly detail: string
  readonly study: string
  /** The ingest job in the store; absent where the source command made none. */
  readonly jobId?: number | null
}

/** The ingest job's standing for one capture; an absent value crosses as `null`. */
export interface CaptureIngestState {
  readonly busyWith?: string | null
  readonly last?: {
    readonly exit: number
    readonly startedAt: string
    readonly finishedAt: string
    readonly logTail: string
  } | null
  readonly detail: string
}

/** How one lane's delta list classes a tolerance rule. */
export interface RuleLaneClass {
  readonly lane: string
  /** accepted | known-bug | unlisted. */
  readonly class: string
  readonly ticket: string
}

/** One count of one rule, under the subdivision it was counted in. */
export interface RuleBucket {
  readonly bucket: string
  readonly hits: number
  readonly cells: number
}

/** What the cheap falsifier came to on one rule: a firing here is a defect. */
export interface RuleOffTier {
  readonly fired: number
  readonly silent: number
  readonly unavailable: number
  readonly evidence: readonly string[]
}

/**
 * One rule of the deployment's roster, and what the corpus came to under it.
 * EVERY rule is a row, whatever it came to: the reading a table of hits alone
 * cannot show is the rule that fires nowhere.
 */
export interface RuleRow {
  /** refusal | tolerance | bless | declaration. */
  readonly family: string
  readonly id: string
  /** pre-run | post-run. */
  readonly stage: string
  /** capture | case | case-document | run-cell | sweep. */
  readonly tier: string
  readonly subject: string
  readonly disposition: string
  /** Why the rule can match nothing; its zero is then its own claim. */
  readonly inert?: string
  /** Why no replay can contradict it — about falsification, never about firing. */
  readonly unfalsifiable?: string
  readonly lanes: readonly RuleLaneClass[]
  /** measured | declared-zero | unexercised | unrostered. */
  readonly standing: string
  readonly hits: number
  readonly cells: number
  readonly buckets: readonly RuleBucket[]
  readonly samples: readonly string[]
  readonly offTier?: RuleOffTier
}

/** What every count was taken over. Each number bounds a zero on the page. */
export interface RulePopulation {
  readonly refusalCaptures: number
  readonly refusalRecords: number
  readonly standingRuns: number
  readonly measuredRuns: number
  readonly cutCaptures: number
  readonly cutCases: number
}

/** The last cheap-falsifier pass, where one left a report. */
export interface RuleOffTierPass {
  readonly at: string
  readonly captures: number
  readonly evaluated: number
  readonly unreadable: number
  readonly findings: number
}

export interface RulesView {
  readonly available: boolean
  readonly detail: string
  readonly lane?: string
  readonly lanes: readonly TriageLane[]
  readonly population: RulePopulation
  readonly offTierPass?: RuleOffTierPass
  readonly rows: readonly RuleRow[]
}

/** One lane the deployment runs, and the ceilings it clamps a request's knobs to (§5 R9). */
export interface LaneView {
  readonly lane: string
  /** A box-wide singleton SUT: one replay at a time, the next waits by name. */
  readonly exclusive: boolean
  /** Cells at once the lane tolerates; null = no ceiling of its own. */
  readonly cellCeiling: number | null
  /** Cell starts per second the lane caps at; null = as fast as slots free. */
  readonly startCeiling: number | null
}

/** The knobs a request carries; a lane clamps them when it runs. */
export interface RequestKnobs {
  readonly concurrency: number
  readonly batchSize: number
  readonly memoryMax: string
  readonly startsPerSecond: number
  readonly cleanup: boolean
}

export interface LanesView {
  readonly lanes: readonly LaneView[]
  /** The knobs a request takes where the page states none. */
  readonly knobs: RequestKnobs
}

/** A selection as the page states it (§5 R1): one form. */
export type RequestSelection =
  | {
    readonly kind: "filter"
    /** The lane the bar's words are read on: the page's. */
    readonly lane?: string
    readonly scope?: string
    readonly q?: string
    readonly rule?: string
    readonly expected?: string
    readonly refusal?: string
    readonly outcome?: string
    readonly feature?: string
    readonly "not-rule"?: readonly string[]
    readonly "not-expected"?: readonly string[]
    readonly "not-refusal"?: readonly string[]
    readonly "not-outcome"?: readonly string[]
    readonly "not-feature"?: readonly string[]
  }
  | { readonly kind: "captures"; readonly captures: readonly string[] }
  | {
    readonly kind: "universe"
    readonly universe: "all" | "standing"
    readonly maxSpan?: number
    readonly greenOn?: string
    readonly sample?: number
    readonly limit?: number
  }
  | {
    readonly kind: "cells"
    readonly cells: readonly { readonly capture: string; readonly callKey?: string; readonly caseId?: string }[]
  }

export interface RequestPreviewCapture {
  readonly capture: string
  readonly cells: number
  /** The keys its cut holds at the prep: what the capture comes to whole. */
  readonly cut: number
  /** full = every key of its cut; cases otherwise. */
  readonly kind: "full" | "cases"
}

export interface RequestLanePreview {
  readonly lane: string
  readonly cells: number
  readonly captures: readonly RequestPreviewCapture[]
  readonly whole: readonly string[]
  readonly unkeyed: number
}

export interface RequestPreview {
  readonly prepId: number
  readonly lanes: readonly RequestLanePreview[]
}

export interface RequestBody {
  readonly lanes: readonly string[]
  readonly selection: RequestSelection
  readonly wholeFirst?: readonly string[]
  readonly knobs?: Partial<RequestKnobs>
}

export interface RequestCreated {
  readonly lane: string
  readonly requestId: number
  readonly cells: number
  readonly captures: readonly string[]
}

/** A job to start (§6 J1): a replay of a request (two jobs with a re-cut first), a retire, an ingest by path. */
export type JobStartBody =
  | {
    readonly kind: "replay"
    readonly requestId: number
    readonly knobs?: {
      readonly concurrency?: number
      readonly batch?: number
      readonly memoryMax?: string
      readonly startsPerSecond?: number
      readonly cleanup?: boolean
      readonly wait?: number
    }
    readonly recutFirst?: "stale-only" | "force"
  }
  | { readonly kind: "retire"; readonly captures: readonly string[] }
  | { readonly kind: "ingest"; readonly paths: readonly string[]; readonly set?: boolean; readonly cut?: boolean }

export interface JobStarted {
  readonly id: number
  readonly kind: string
  readonly label: string
  readonly logPath: string
}

export type JobStatus = "queued" | "running" | "done" | "failed" | "interrupted" | "died"

/** A job as the list shows it (§6 J8): the row's facts the page reads, its units landed over total, failed, gone. */
export interface JobListed {
  readonly id: number
  readonly kind: string
  readonly status: JobStatus
  readonly label: string
  readonly lane: string | null
  readonly requestId: number | null
  readonly createdAt: string
  readonly startedAt: string | null
  readonly finishedAt: string | null
  readonly exit: number | null
  /** The lock holder's line while the job waits for it. */
  readonly waitingOn: string | null
  readonly batchAt: number | null
  readonly batches: number | null
  readonly stopRequestedAt: string | null
  readonly restartOf: number | null
  readonly mode: "resume" | "again" | null
  readonly landed: number
  readonly total: number
  readonly failed: number
  readonly gone: number
}

/** One page of the queue: running rows first, then queued, then the ended newest first. */
export interface JobsPage {
  readonly rows: readonly JobListed[]
  readonly total: number
  readonly limit: number
  readonly offset: number
}

/** The progress query's counts (§6 J3). */
export interface JobProgress {
  readonly total: number
  readonly landed: number
  readonly pending: number
  readonly goneRefused: number
  readonly goneMissing: number
  readonly passed: number
  readonly failed: number
  readonly skipped: number
  readonly runs: number
  readonly openRuns: number
}

export interface JobKin {
  readonly id: number
  readonly status: string
  readonly mode: "resume" | "again" | null
}

/** One job: its row, its progress, the request it runs and the restart chain around it. */
export interface JobDetail {
  readonly id: number
  readonly kind: string
  readonly status: JobStatus
  readonly label: string
  readonly lane: string | null
  readonly createdAt: string
  readonly createdBy: string
  readonly pid: number | null
  readonly startedAt: string | null
  readonly finishedAt: string | null
  readonly exit: number | null
  readonly logPath: string | null
  readonly hooksPath: string | null
  readonly waitingOn: string | null
  readonly sweepId: number | null
  readonly batchAt: number | null
  readonly batches: number | null
  readonly stopRequestedAt: string | null
  readonly stopGrace: number | null
  readonly restartOf: number | null
  readonly mode: "resume" | "again" | null
  readonly planned: number | null
  readonly args: Readonly<Record<string, unknown>>
  readonly progress: JobProgress
  readonly request: {
    readonly id: number
    readonly lane: string
    readonly cells: number
    readonly captures: number
    /** The selection as the request command would state it. */
    readonly selection: string
  } | null
  readonly lineage: { readonly ancestors: readonly JobKin[]; readonly descendants: readonly JobKin[] }
}

/** A byte range of the job's log: the next poll starts at `to`; `done` once the job ended and the end was reached. */
export interface JobLogRange {
  readonly from: number
  readonly to: number
  readonly text: string
  readonly done: boolean
}

export interface JobGoneCell {
  readonly capture: string
  readonly callKey: string
  readonly outcome: string
  readonly reasons: readonly string[]
}

export interface JobRedCell {
  readonly capture: string
  readonly lane: string
  readonly caseId: string
  readonly runId: number
  readonly error: TriageMainError | null
  /** The body rows no rule accepts, whatever `error` names. */
  readonly bodyFailing: number
}

export type JobCells =
  | { readonly outcome: "gone"; readonly cells: readonly JobGoneCell[]; readonly total: number }
  | { readonly outcome: "red"; readonly cells: readonly JobRedCell[]; readonly total: number }

/** What deleting the job's runs takes (§7 K4): the page confirms once on these counts. */
export interface JobDeletePlan {
  readonly runs: number
  readonly rows: { readonly runs: number; readonly caseResults: number; readonly runRuleHits: number }
  readonly bundles: number
  readonly held: readonly { readonly id: number; readonly why: string }[]
}

export interface JobHook {
  readonly name: string
  readonly text: string
}

export interface SourceStanding {
  readonly lane: string
  readonly runId: number
  readonly status: string
  readonly cases: number
  readonly failed: number
  readonly skipped: number
  readonly finishedAt: string | null
}

/** One capture of the source list (§3 S5–S6). */
export interface SourceRow {
  readonly capture: string
  readonly sourcePath: string | null
  readonly addedAt: string
  readonly ingestedAt: string | null
  /** current | stale | failed. */
  readonly ingest: string
  readonly ingestFailed: string | null
  readonly prepId: number | null
  readonly cutAt: string | null
  /** current | stale | broke | never. */
  readonly cut: string
  readonly cutBroke: string | null
  readonly cases: number
  readonly refusals: number
  readonly spanSeconds: number | null
  readonly standings: readonly SourceStanding[]
}

export interface SourcesPage {
  readonly rows: readonly SourceRow[]
  readonly total: number
  readonly limit: number
  readonly offset: number
  /** The roots a path may be added from. */
  readonly roots: readonly string[]
}

/** What retiring one capture takes (§3 S4), read before the one confirmation. */
export interface RetirePlan {
  readonly capture: string
  readonly rows: Readonly<Record<string, number>>
  readonly runs: number
  readonly bundles: number
  readonly sessionDir: string | null
  readonly artifactDir: string | null
  readonly uploads: readonly string[]
  readonly holders: readonly { readonly jobId: number; readonly kind: string; readonly how: string }[]
  readonly known: boolean
}

export interface ServerConfig {
  readonly configPath: string
  readonly caseRoots: readonly { readonly id: string; readonly label: string; readonly path: string }[]
  readonly runRoots: readonly { readonly id: string; readonly label: string; readonly path: string }[]
  readonly flowsRoots: readonly { readonly id: string; readonly label: string; readonly path: string }[]
  /** The triage deployment, where one is configured: the drop zone reads its lane. */
  readonly triage?: { readonly lane: string; readonly repoRoot: string; readonly corpus: string }
  readonly port: number
  readonly staticDir?: string
  /** The deployment's command lines a view offers for copying (`commands.ts`). */
  readonly commands?: { readonly triage?: string; readonly request?: string; readonly job?: string }
}

/** The query string of the fields that carry a value, a list repeating its key per entry; "" where none do. */
const search = (params: Record<string, string | number | ReadonlyArray<string> | undefined>): string => {
  const query = new URLSearchParams()
  for (const [key, value] of Object.entries(params)) {
    if (Array.isArray(value)) {
      for (const one of value) query.append(key, one)
    } else if (value !== undefined && value !== "") query.set(key, String(value))
  }
  const text = query.toString()
  return text === "" ? "" : `?${text}`
}

/** A request whose failure, of any status, is the server's stated reason where it states one. */
const request = async <A>(path: string, method: "GET" | "POST" = "GET"): Promise<A> => {
  const response = await fetch(path, { method, headers: { accept: "application/json" } })
  if (!response.ok) throw new Error(await said(response))
  return await response.json() as A
}

/** A failure the server states: its `detail` or `reason` is the message, not the status line. */
const said = async (response: Response): Promise<string> => {
  const text = await response.text()
  try {
    const parsed = JSON.parse(text) as { detail?: unknown; reason?: unknown }
    if (typeof parsed.detail === "string") return parsed.detail
    if (typeof parsed.reason === "string") return parsed.reason
  } catch {
    // Not JSON: the text is the message.
  }
  return `${response.status} ${response.statusText} — ${text.slice(0, 400)}`
}

/** A JSON POST: the body as the API's own schema states it. */
const post = async <A>(path: string, body: unknown): Promise<A> => {
  const response = await fetch(path, {
    method: "POST",
    body: JSON.stringify(body),
    headers: { accept: "application/json", "content-type": "application/json" }
  })
  if (!response.ok) throw new Error(await said(response))
  return await response.json() as A
}

/**
 * A multipart POST: the capture itself crosses as a file, so the JSON helper
 * cannot carry it. The browser sets the boundary — naming a content-type here
 * would strip it and the server would decode nothing.
 */
const upload = async <A>(path: string, form: FormData): Promise<A> => {
  const response = await fetch(path, { method: "POST", body: form, headers: { accept: "application/json" } })
  if (!response.ok) {
    const detail = await response.text()
    throw new Error(`${response.status} ${response.statusText} — ${detail.slice(0, 400)}`)
  }
  return await response.json() as A
}

/** A GET whose refusal (400) says why: the reason is the message. */
const ask = async <A>(path: string): Promise<A> => {
  const response = await fetch(path, { headers: { accept: "application/json" } })
  if (!response.ok) throw new Error(await said(response))
  return await response.json() as A
}

/** What a run-set comparison is asked for: the two selectors, and how much of it. */
export interface CompareQuery {
  readonly a: string
  readonly b: string
  /** The lane a side that pins none is read on; absent = the server's. */
  readonly lane?: string | undefined
  /** Every compared call, the unchanged ones too. */
  readonly all?: boolean | undefined
}

export const api = {
  config: () => request<ServerConfig>("/api/config"),
  /** The `sip.compare/1` document of two run sets. */
  compare: (query: CompareQuery) =>
    ask<CompareReport.CompareReport>(
      `/api/compare${search({ a: query.a, b: query.b, lane: query.lane, all: query.all === true ? "1" : undefined })}`
    ),
  /** The `sip.run-report/1` document of one run set. */
  report: (run: string, lane?: string) => ask<RunReport.RunReport>(`/api/report${search({ run, lane })}`),
  cases: () => request<readonly CaseSummary[]>("/api/cases"),
  case: (id: string, variant?: string) =>
    request<CaseDetail>(
      `/api/cases/${encodeURIComponent(id)}${variant ? `?variant=${encodeURIComponent(variant)}` : ""}`
    ),
  caseSource: (id: string, variant?: string) =>
    request<CaseSource>(
      `/api/cases/${encodeURIComponent(id)}/source${variant ? `?variant=${encodeURIComponent(variant)}` : ""}`
    ),
  caseSourceLegs: (id: string, variant: string | undefined, from: number, count: number) =>
    request<FlowsLegs>(
      `/api/cases/${encodeURIComponent(id)}/source/legs${search({ variant, from, count })}`
    ),
  runs: () => request<readonly RunSummary[]>("/api/runs"),
  run: (id: string) => request<RunDetail>(`/api/runs/${encodeURIComponent(id)}`),
  triageQueue: (query: TriageQuery = {}) =>
    request<TriageQueue>(
      `/api/triage${
        search({
          lane: query.lane,
          scope: query.scope,
          q: query.q,
          rule: query.rule,
          expected: query.expected,
          refusal: query.refusal,
          outcome: query.outcome,
          feature: query.feature,
          "not-rule": query["not-rule"],
          "not-expected": query["not-expected"],
          "not-refusal": query["not-refusal"],
          "not-outcome": query["not-outcome"],
          "not-feature": query["not-feature"],
          limit: query.limit
        })
      }`
    ),
  /** The delta sheet of the queue slice `query` names; every kept call is read, so no limit. */
  deltaSheet: (query: Omit<TriageQuery, "limit"> = {}) =>
    request<DeltaSheet>(
      `/api/deltas${
        search({
          lane: query.lane,
          scope: query.scope,
          q: query.q,
          rule: query.rule,
          expected: query.expected,
          refusal: query.refusal,
          outcome: query.outcome,
          feature: query.feature,
          "not-rule": query["not-rule"],
          "not-expected": query["not-expected"],
          "not-refusal": query["not-refusal"],
          "not-outcome": query["not-outcome"],
          "not-feature": query["not-feature"]
        })
      }`
    ),
  /** One call's recent runs across every lane, newest first. */
  triageCallHistory: (capture: string, caseId: string) =>
    request<TriageCallHistory>(`/api/triage/${encodeURIComponent(capture)}/history${search({ case: caseId })}`),
  /** One capture's study, or — with `caseId` — the study of that one call of it. */
  triageStudy: (capture: string, lane?: string, caseId?: string) =>
    request<TriageStudy>(`/api/triage/${encodeURIComponent(capture)}${search({ lane, case: caseId })}`),
  triageFlowsEnvelope: (capture: string) =>
    request<FlowsEnvelope>(`/api/triage/${encodeURIComponent(capture)}/flows/envelope`),
  triageFlowsLegs: (capture: string, from: number, count: number) =>
    request<FlowsLegs>(
      `/api/triage/${encodeURIComponent(capture)}/flows/legs${search({ from, count })}`
    ),
  triageCell: (capture: string, session: string, cell: string) =>
    request<TriageStudyCellDetail>(
      `/api/triage/${encodeURIComponent(capture)}/cells/${encodeURIComponent(session)}/${
        encodeURIComponent(cell)
      }`
    ),
  /** One capture: one file, or the consecutive files of one tap together. */
  captureIngest: (files: ReadonlyArray<File>, lane?: string) => {
    const form = new FormData()
    for (const file of files) form.append("file", file, file.name)
    if (lane !== undefined && lane !== "") form.append("lane", lane)
    return upload<CaptureIngestStart>("/api/captures", form)
  },
  captureIngestState: (capture: string) =>
    request<CaptureIngestState>(`/api/captures/${encodeURIComponent(capture)}/ingest`),
  rules: (lane?: string) => request<RulesView>(`/api/rules${search({ lane })}`),
  lanes: () => request<LanesView>("/api/lanes"),
  requestPreview: (body: RequestBody) => post<RequestPreview>("/api/requests/preview", body),
  requestCreate: (body: RequestBody) =>
    post<{ readonly created: readonly RequestCreated[] }>("/api/requests", body),
  jobStart: (body: JobStartBody) => post<{ readonly jobs: readonly JobStarted[] }>("/api/jobs", body),
  jobs: (query: { status?: string; limit?: number; offset?: number } = {}) =>
    request<JobsPage>(`/api/jobs${search({ status: query.status, limit: query.limit, offset: query.offset })}`),
  job: (id: number) => request<JobDetail>(`/api/jobs/${id}`),
  jobLog: (id: number, from: number) => request<JobLogRange>(`/api/jobs/${id}/log${search({ from })}`),
  jobCells: (id: number, outcome: "gone" | "red", limit?: number) =>
    request<JobCells>(`/api/jobs/${id}/cells${search({ outcome, limit })}`),
  jobStop: (id: number, grace?: number) => post<{ readonly stopped: number }>(`/api/jobs/${id}/stop`, grace === undefined ? {} : { grace }),
  jobRestart: (id: number, mode: "resume" | "again") => post<JobStarted>(`/api/jobs/${id}/restart`, { mode }),
  jobDeletePlan: (id: number) => request<JobDeletePlan>(`/api/jobs/${id}/delete-plan`),
  jobDelete: (id: number) => post<JobStarted>(`/api/jobs/${id}/delete`, {}),
  jobHooks: (id: number) => request<readonly JobHook[]>(`/api/jobs/${id}/hooks`),
  sources: (query: { stale?: boolean; q?: string; limit?: number; offset?: number }) =>
    request<SourcesPage>(
      `/api/sources${search({ stale: query.stale === true ? "1" : undefined, q: query.q, limit: query.limit, offset: query.offset })}`
    ),
  retirePlan: (capture: string) => request<RetirePlan>(`/api/sources/${encodeURIComponent(capture)}/retire-plan`)
}

export type Async<A> =
  | { readonly state: "loading" }
  | { readonly state: "ok"; readonly value: A }
  | { readonly state: "error"; readonly message: string }

/** Runs a request when its key changes, and drops results that arrived late. */
export const useAsync = <A>(load: () => Promise<A>, key: string): Async<A> => {
  const [result, setResult] = useState<Async<A>>({ state: "loading" })
  useEffect(() => {
    let live = true
    setResult({ state: "loading" })
    load()
      .then((value) => {
        if (live) setResult({ state: "ok", value })
      })
      .catch((error: unknown) => {
        if (live) setResult({ state: "error", message: error instanceof Error ? error.message : String(error) })
      })
    return () => {
      live = false
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key])
  return result
}
