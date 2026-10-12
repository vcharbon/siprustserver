/**
 * The triage queue: what the DB says is failing, decorated by what the git
 * registry says it MEANS.
 *
 * The enumeration is live run status — a capture leaves the queue the moment a
 * run of it comes back green, with no human writing anything. The registry is
 * an overlay on top: it explains a failure (accepted behavior, or a bug under a
 * ticket), and it also earns a row of its own when a capture it still calls
 * pending has started passing — that row is the close-the-bugs prompt.
 *
 * Pure: the caller reads the standings, the registry and the ticket files, so
 * the sweep and the viewer rule identically.
 */
import * as Triage from "./registry.js"
import type { CaseOutcome, CaseRun, Deltas, Standing } from "./model.js"
import { isGreen, isSkipped, NO_DELTAS } from "./model.js"

/**
 * Why a capture is on the queue. The four `failing-*` values are the live
 * failure crossed with the registry's ruling; `passing-but-pending` is the
 * opposite case — a standing pending record whose capture now passes. Two
 * values take no triage at all and sort last: `skipped`, where the document
 * declared the lane blocked so the run never asked the SUT anything, and
 * `no-cases`, where the CUT refused every call of the capture — the run has no
 * replay to rule on, and the refusal states its own reason in `excluded.json`.
 * `passing` is the lane's green mass, which is triage work for nobody and is
 * enumerated only when the caller asks for it (`includePassing`).
 */
export type QueueStatus =
  | "failing-untriaged"
  | "failing-requeue"
  | "failing-suppressed"
  | "failing-excluded"
  | "passing-but-pending"
  | "skipped"
  | "no-cases"
  | "passing"
  | "off-queue"

export const QUEUE_ORDER: ReadonlyArray<QueueStatus> = [
  "failing-untriaged",
  "failing-requeue",
  "failing-suppressed",
  "passing-but-pending",
  "skipped",
  "no-cases",
  "failing-excluded",
  "passing"
]

/**
 * The statuses a triage session can actually take. Everything else is handled —
 * a final verdict already rules on it, the lane was declared blocked, or the cut
 * refused every call — or green; a queue whose top rows cannot be worked is not
 * a queue, so this is the viewer's default filter.
 */
export const TAKEABLE: ReadonlyArray<QueueStatus> = [
  "failing-untriaged",
  "failing-requeue",
  "passing-but-pending"
]

/** The scope that serves the takeable statuses: the viewer's default. */
export const QUEUE_SCOPE_TAKEABLE = "takeable"
/** The scope that serves every capture with a call, green included. */
export const QUEUE_SCOPE_ALL = "all"

/**
 * A row with a call to look at: anything but a cut the CUT refused every call
 * of. A run the driver crashed out of before it ran a case still has its
 * calls, and is triage work.
 */
const hasCall = (row: Row): boolean => row.status !== "no-cases"

/**
 * The rows a scope keeps. `all` is every capture with a call, green included,
 * and `takeable` the takeable statuses among them: a refused cut is listed
 * only under its own status, never among the lane's calls. Any other scope
 * is one named status.
 */
export const inScope = (scope: string) => (row: Row): boolean =>
  scope === QUEUE_SCOPE_ALL
    ? hasCall(row)
    : scope === QUEUE_SCOPE_TAKEABLE
    ? hasCall(row) && TAKEABLE.includes(row.status)
    : row.status === scope

export interface TicketView {
  readonly path: string
  readonly state: Triage.TicketState
}

export interface Row {
  readonly capture: string
  readonly lane: string
  readonly status: QueueStatus
  /** The contracts checker's ruling on the registry record, or `untriaged`. */
  readonly disposition: Triage.Disposition
  readonly verdict?: string
  readonly ticket?: TicketView
  readonly decidedOn?: string
  /** Case ids the LATEST run actually ran; empty for a capture never run. */
  readonly cases: ReadonlyArray<string>
  /**
   * Steps the latest run's failing cases died at. THIS is the defect, and it is
   * the queue's first sort of what a row will cost: a row with a failed step
   * has a replay that stopped, whatever `deltas` says.
   */
  readonly failedSteps: ReadonlyArray<string>
  /**
   * The distinct lane verdicts the latest run's skipped cases were blocked by,
   * verbatim (`blocked:<reason>`). THIS is why a `skipped` row is not red.
   */
  readonly skipReasons: ReadonlyArray<string>
  /**
   * The latest run's cases, each as itself. A capture that cut several calls is
   * several questions, and only this says which call died where — the flat
   * `failedSteps` above cannot be read back onto a call.
   */
  readonly caseResults: ReadonlyArray<CaseOutcome>
  /** Gating findings of the latest run's post-run RFC audit: a SUT violation the scripted peers tolerated. */
  readonly rfcGating: number
  /** The latest run's confrontation classification, summed over its cases. */
  readonly deltas: Deltas
  readonly runId?: number
  readonly runStatus?: Standing["status"]
  readonly failed?: number
  readonly finishedAt?: string
  readonly bundlePath?: string
  readonly lastOkRunId?: number
  readonly lastOkBundlePath?: string
  /**
   * Each case's current run, of any set kind: the capture reads off `runId`,
   * the call off its entry here.
   */
  readonly caseRuns: ReadonlyArray<CaseRun>
}

export interface Overlay {
  readonly rows: ReadonlyArray<Row>
  /** Registry records that earn no queue row — never lost, just not takeable. */
  readonly offQueue: ReadonlyArray<Row>
  readonly totals: Readonly<Record<string, number>>
  /** Captures in the corpus universe with no run on this lane at all. */
  readonly neverRun: ReadonlyArray<string>
}

export interface OverlayInput {
  readonly standings: ReadonlyArray<Standing>
  readonly registry: Triage.Registry
  /** Ticket path → state, resolved from disk by the caller. */
  readonly tickets: ReadonlyMap<string, Triage.TicketState>
  /** The corpus universe, for the never-run count; omit to skip it. */
  readonly captures?: ReadonlyArray<string>
  /**
   * Enumerate the green captures too, as `passing` rows. The queue proper is
   * what needs triage, so this is off by default; a viewer showing the lane
   * whole turns it on.
   */
  readonly includePassing?: boolean
}

const ticketOf = (
  record: Triage.RegistryRecord | undefined,
  tickets: ReadonlyMap<string, Triage.TicketState>
): TicketView | undefined => {
  if (record === undefined || Triage.isFinal(record)) return undefined
  return { path: record.ticket, state: tickets.get(record.ticket) ?? "missing" }
}

const decorate = (
  record: Triage.RegistryRecord | undefined,
  ticket: TicketView | undefined
): Pick<Row, "verdict" | "ticket" | "decidedOn"> => ({
  ...(record === undefined ? {} : { verdict: record.verdict, decidedOn: record.decided_on }),
  ...(ticket === undefined ? {} : { ticket })
})

const failingStatus = (disposition: Triage.Disposition): QueueStatus =>
  disposition === "excluded"
    ? "failing-excluded"
    : disposition === "requeue"
    ? "failing-requeue"
    : disposition === "suppressed"
    ? "failing-suppressed"
    : "failing-untriaged"

export const overlay = (input: OverlayInput): Overlay => {
  const rows: Array<Row> = []
  const seen = new Set<string>()
  for (const standing of input.standings) {
    seen.add(standing.capture)
    const record = input.registry[standing.capture]
    const ticket = ticketOf(record, input.tickets)
    const disposition = Triage.disposition(record, ticket?.state)
    const green = isGreen(standing)
    const skipped = isSkipped(standing)
    // A cut that refused every call outranks the registry's ruling: there is no
    // replay under it to call excluded or untriaged, whatever a record says.
    const noCases = standing.status === "no-cases"
    const pending = record !== undefined && !Triage.isFinal(record)
    if (green && !pending && input.includePassing !== true) continue
    const common: Row = {
      capture: standing.capture,
      lane: standing.lane,
      status: green
        ? (pending ? "passing-but-pending" : "passing")
        : skipped
        ? "skipped"
        : noCases
        ? "no-cases"
        : failingStatus(disposition),
      disposition,
      ...decorate(record, ticket),
      cases: standing.caseIds,
      failedSteps: standing.failedSteps,
      skipReasons: standing.skipReasons,
      caseResults: standing.caseResults,
      rfcGating: standing.rfcGating,
      deltas: standing.deltas,
      runId: standing.runId,
      runStatus: standing.status,
      failed: standing.failed,
      ...(standing.finishedAt === null ? {} : { finishedAt: standing.finishedAt }),
      bundlePath: standing.bundlePath,
      ...(standing.lastOkRunId === null ? {} : { lastOkRunId: standing.lastOkRunId }),
      ...(standing.lastOkBundlePath === null ? {} : { lastOkBundlePath: standing.lastOkBundlePath }),
      caseRuns: standing.caseRuns
    }
    rows.push(common)
  }
  const order = new Map(QUEUE_ORDER.map((status, index) => [status, index]))
  rows.sort((a, b) => {
    const byStatus = (order.get(a.status) ?? 99) - (order.get(b.status) ?? 99)
    return byStatus !== 0 ? byStatus : a.capture < b.capture ? -1 : a.capture > b.capture ? 1 : 0
  })

  const onQueue = new Set(rows.map((row) => row.capture))
  const offQueue: Array<Row> = []
  for (const capture of Object.keys(input.registry).sort()) {
    if (onQueue.has(capture)) continue
    const record = input.registry[capture]!
    const ticket = ticketOf(record, input.tickets)
    offQueue.push({
      capture,
      lane: "",
      status: "off-queue",
      disposition: Triage.disposition(record, ticket?.state),
      ...decorate(record, ticket),
      cases: [],
      failedSteps: [],
      skipReasons: [],
      caseResults: [],
      rfcGating: 0,
      deltas: NO_DELTAS,
      caseRuns: []
    })
  }

  const totals: Record<string, number> = {}
  for (const row of rows) totals[row.status] = (totals[row.status] ?? 0) + 1

  const neverRun = (input.captures ?? []).filter((capture) => !seen.has(capture))
  return { rows, offQueue, totals, neverRun }
}

/** The `--known` lane's work list: pending registry records plus current failures. */
export const knownCaptures = (input: OverlayInput): ReadonlyArray<string> => {
  const wanted = new Set<string>()
  for (const [capture, record] of Object.entries(input.registry)) {
    if (!Triage.isFinal(record)) wanted.add(capture)
  }
  for (const standing of input.standings) {
    if (!isGreen(standing) && !isSkipped(standing)) wanted.add(standing.capture)
  }
  return [...wanted].sort()
}

