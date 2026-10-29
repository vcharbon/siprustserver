/**
 * The machine-readable **load run index** (`load-result.json`), mirroring
 * `e2e_model::loadrun`: one loadgen run's result as a single authored-shape
 * document, so the e2e website renders a load run beside a functional campaign
 * run without re-deriving anything.
 *
 * Every path in it is RELATIVE to the run directory, so a run dir is
 * self-contained and portable. camelCase on the wire, declaration-order bytes —
 * the same discipline as the campaign records.
 */
import * as Schema from "effect/Schema"
import { formatDeclared } from "./canonical.js"
import { defaulted } from "./serde.js"
import { STRICT } from "./strict.js"

/** The sampling gate's bounds, as the run was configured. */
export const Sampling = Schema.Struct({
  sampleCap: Schema.Int,
  recordEvery: Schema.Int,
  chaseCalls: Schema.Int
})
export interface Sampling extends Schema.Schema.Type<typeof Sampling> {}

/** The recorded calls a failure row's class judges on their recording. */
export const JudgedBasis = Schema.Literals(["audited", "checked"])

/** A failure row's share of the recorded calls its class judged, and the estimate base. */
export const JudgedShare = Schema.Struct({
  failed: Schema.Int,
  judged: Schema.Int,
  base: Schema.Int,
  basis: JudgedBasis
})
export interface JudgedShare extends Schema.Schema.Type<typeof JudgedShare> {}

/** The run's headline figures as of the doc. */
export const Progress = Schema.Struct({
  elapsedMs: Schema.Int,
  offeringMs: Schema.Int,
  targetCps: Schema.Number,
  draining: Schema.Boolean,
  offered: Schema.Int,
  placed: Schema.Int,
  inFlight: Schema.Int,
  ok: Schema.Int,
  failed: Schema.Int,
  rejected: Schema.Int,
  generatorFailures: Schema.Int,
  retransmissionsReceived: Schema.Int,
  retransmissionsResent: Schema.Int,
  retransmitEngine: Schema.Boolean
})
export interface Progress extends Schema.Schema.Type<typeof Progress> {}

const NO_PROGRESS = {
  elapsedMs: 0,
  offeringMs: 0,
  targetCps: 0,
  draining: false,
  offered: 0,
  placed: 0,
  inFlight: 0,
  ok: 0,
  failed: 0,
  rejected: 0,
  generatorFailures: 0,
  retransmissionsReceived: 0,
  retransmissionsResent: 0,
  retransmitEngine: false
}

/** One window of the run's time series (milliseconds since the run started). */
export const SeriesPoint = Schema.Struct({
  startMs: Schema.Int,
  spanMs: Schema.Int,
  placed: Schema.Int,
  ok: Schema.Int,
  failed: Schema.Int,
  rejected: Schema.Int,
  retransmissionsReceived: Schema.Int,
  retransmissionsResent: Schema.Int,
  p50Ms: Schema.Number,
  p99Ms: Schema.Number
})
export interface SeriesPoint extends Schema.Schema.Type<typeof SeriesPoint> {}

/** One change of the offered rate, or the drain. */
export const RateEvent = Schema.Struct({
  atMs: Schema.Int,
  kind: Schema.Literals(["rate", "drain"]),
  fromCps: Schema.Number,
  toCps: Schema.Number
})
export interface RateEvent extends Schema.Schema.Type<typeof RateEvent> {}

/** One failed call among the run's last failures. */
export const FailureRow = Schema.Struct({
  atMs: Schema.Int,
  scenario: Schema.String,
  class: Schema.String,
  case: defaulted(Schema.String, ""),
  page: Schema.optionalKey(Schema.String)
})
export interface FailureRow extends Schema.Schema.Type<typeof FailureRow> {}

/** A result class and how many calls ended in it. */
export const ClassCount = Schema.Struct({ class: Schema.String, count: Schema.Int })
export interface ClassCount extends Schema.Schema.Type<typeof ClassCount> {}

/** One arrow of a message ladder, merged over the calls. */
export const LadderArrow = Schema.Struct({
  from: Schema.String,
  to: Schema.String,
  label: Schema.String,
  nth: Schema.Int,
  count: Schema.Int,
  retransmissions: Schema.Int,
  resent: Schema.Int,
  meanOffsetMs: Schema.Number,
  errors: Schema.optionalKey(Schema.Array(ClassCount))
})
export interface LadderArrow extends Schema.Schema.Type<typeof LadderArrow> {}

/** One scenario's message ladder. */
export const Ladder = Schema.Struct({
  scenario: Schema.String,
  calls: Schema.Int,
  engine: Schema.Boolean,
  lanes: Schema.Array(Schema.String),
  arrows: Schema.Array(LadderArrow),
  other: Schema.optionalKey(LadderArrow),
  afterCall: Schema.optionalKey(Schema.Array(ClassCount)),
  beforeReply: Schema.optionalKey(Schema.Array(ClassCount))
})
export interface Ladder extends Schema.Schema.Type<typeof Ladder> {}

/** Per scenario: the calls placed, recorded and judged on their recording. */
export const ScenarioRecording = Schema.Struct({
  scenario: Schema.String,
  placed: Schema.Int,
  recorded: Schema.Int,
  accepted: Schema.Int,
  audited: Schema.Int
})
export interface ScenarioRecording extends Schema.Schema.Type<typeof ScenarioRecording> {}

/** Run metadata: timing and the echoed run knobs. */
export const LoadRunMeta = Schema.Struct({
  /** Wall-clock start, Unix epoch milliseconds. */
  startedMs: Schema.Int,
  /** Wall-clock finish (the moment this doc was written), Unix epoch ms. */
  finishedMs: Schema.Int,
  /** `false` while a periodic snapshot is written mid-run; `true` at run end. */
  finished: Schema.Boolean,
  /** The SUT ingress the INVITEs were routed through (the `lb`/VIP address). */
  target: Schema.String,
  /** Offered call rate the run was configured for (calls/s). */
  cps: Schema.Number,
  durationSecs: Schema.Int,
  /** Max concurrent in-flight calls (offered load above this was shed). */
  maxInFlight: Schema.Int,
  /** The egress policy label the run realized its b-leg with. */
  egress: Schema.optionalKey(Schema.String),
  profile: Schema.optionalKey(Schema.String),
  /** The load generator's command line, as typed (program first). */
  commandLine: Schema.optionalKey(Schema.Array(Schema.String)),
  /** The sampling gate's bounds: which calls ran recorded. */
  sampling: Schema.optionalKey(Sampling)
})
export interface LoadRunMeta extends Schema.Schema.Type<typeof LoadRunMeta> {}

/** One `(scenario, class, case)` completed-call count. */
export const CountRow = Schema.Struct({
  scenario: Schema.String,
  /** The result class label (`ok`, `timeout`, `status_503`, `check_fail`, …). */
  class: Schema.String,
  /** The bounded case discriminator refining the class. Empty = un-refined. */
  case: defaulted(Schema.String, ""),
  count: Schema.Int,
  /** `true` iff the class is no failure (`ok`). */
  ok: Schema.Boolean,
  /** The row's share of the calls its class judges on their recording. */
  judged: Schema.optionalKey(JudgedShare),
  /** The first stored sample's one-line failure detail. */
  error: Schema.optionalKey(Schema.String)
})
export interface CountRow extends Schema.Schema.Type<typeof CountRow> {}

/** Per-scenario end-to-end latency summary (milliseconds). */
export const LatencyRow = Schema.Struct({
  scenario: Schema.String,
  n: Schema.Int,
  meanMs: Schema.Number,
  p50Ms: Schema.Number,
  p90Ms: Schema.Number,
  p99Ms: Schema.Number,
  maxMs: Schema.Number
})
export interface LatencyRow extends Schema.Schema.Type<typeof LatencyRow> {}

/** Per-`(scenario, checkpoint)` named-checkpoint latency summary (milliseconds). */
export const CheckpointRow = Schema.Struct({
  scenario: Schema.String,
  checkpoint: Schema.String,
  n: Schema.Int,
  p50Ms: Schema.Number,
  p90Ms: Schema.Number,
  p99Ms: Schema.Number
})
export interface CheckpointRow extends Schema.Schema.Type<typeof CheckpointRow> {}

/** Per-`(scenario, exchange)` SIP round-trip summary (milliseconds). */
export const RttRow = Schema.Struct({
  scenario: Schema.String,
  exchange: Schema.String,
  n: Schema.Int,
  p50Ms: Schema.Number,
  p90Ms: Schema.Number,
  p99Ms: Schema.Number
})
export interface RttRow extends Schema.Schema.Type<typeof RttRow> {}

/** Per-scenario check-verdict tally over the sampled calls. */
export const CheckSummaryRow = Schema.Struct({
  scenario: Schema.String,
  passed: Schema.Int,
  failed: Schema.Int
})
export interface CheckSummaryRow extends Schema.Schema.Type<typeof CheckSummaryRow> {}

/** The run's cross-call health canaries — the first things a triager scans. */
export const Canaries = Schema.Struct({
  /** Inbound datagrams that matched no live call; should be ~0 in a clean run. */
  orphans: Schema.Int,
  /** Offered calls dropped at the max-in-flight cap. */
  shed: Schema.Int,
  /** Inbound datagrams discarded by the simulated packet-loss model. */
  dropsIn: Schema.Int,
  /** Outbound datagrams discarded by the simulated packet-loss model. */
  dropsOut: Schema.Int,
  /** Correlated datagrams discarded because their call's inbox was full. */
  inboxDrops: defaulted(Schema.Int, 0),
  /** Calls that reached the ring→answer step (the 18x-delivery denominator). */
  ringingExpected: Schema.Int,
  /** Of those, how many saw the 18x ringing provisional. A RATE gated at >99%. */
  ringingReceived: Schema.Int
})
export interface Canaries extends Schema.Schema.Type<typeof Canaries> {}

/** The 18x ringing-delivery ratio in `[0,1]` (`1.0` when nothing rang). */
export const ringingRatio = (canaries: Canaries): number =>
  canaries.ringingExpected === 0 ? 1 : canaries.ringingReceived / canaries.ringingExpected

/** The stored sampled callflow pages for one `(scenario, class, case)` bucket. */
export const SampleGroup = Schema.Struct({
  scenario: Schema.String,
  class: Schema.String,
  case: defaulted(Schema.String, ""),
  /** Run-dir-relative paths to each sample's rendered callflow HTML page. */
  pages: Schema.Array(Schema.String)
})
export interface SampleGroup extends Schema.Schema.Type<typeof SampleGroup> {}

/** One load run's complete result, as persisted to `load-result.json`. */
export const LoadRunIndex = Schema.Struct({
  meta: LoadRunMeta,
  progress: defaulted(Progress, NO_PROGRESS),
  series: Schema.optionalKey(Schema.Array(SeriesPoint)),
  rateEvents: Schema.optionalKey(Schema.Array(RateEvent)),
  rateEventsDropped: Schema.optionalKey(Schema.Int),
  recentFailures: Schema.optionalKey(Schema.Array(FailureRow)),
  recording: Schema.optionalKey(Schema.Array(ScenarioRecording)),
  ladders: Schema.optionalKey(Schema.Array(Ladder)),
  counts: Schema.Array(CountRow),
  latency: Schema.Array(LatencyRow),
  checkpoints: Schema.optionalKey(Schema.Array(CheckpointRow)),
  rtt: Schema.optionalKey(Schema.Array(RttRow)),
  checks: Schema.optionalKey(Schema.Array(CheckSummaryRow)),
  canaries: Canaries,
  samples: Schema.optionalKey(Schema.Array(SampleGroup))
})
export interface LoadRunIndex extends Schema.Schema.Type<typeof LoadRunIndex> {}

export const decodeLoadRunIndex = Schema.decodeUnknownEffect(LoadRunIndex, STRICT)
export const decodeLoadRunIndexSync = Schema.decodeUnknownSync(LoadRunIndex, STRICT)

/** Total completed calls across every bucket. */
export const totalCalls = (index: LoadRunIndex): number => index.counts.reduce((sum, row) => sum + row.count, 0)

/** Completed calls that were NOT `ok`. */
export const failedCalls = (index: LoadRunIndex): number =>
  index.counts.filter((row) => !row.ok).reduce((sum, row) => sum + row.count, 0)

// --- Declaration-order emission ----------------------------------------------

const pruned = (value: Record<string, unknown>): Record<string, unknown> =>
  Object.fromEntries(Object.entries(value).filter(([, v]) => v !== undefined))

const classCountJson = (c: ClassCount) => ({ class: c.class, count: c.count })

const arrowJson = (a: LadderArrow): Record<string, unknown> =>
  pruned({
    from: a.from,
    to: a.to,
    label: a.label,
    nth: a.nth,
    count: a.count,
    retransmissions: a.retransmissions,
    resent: a.resent,
    meanOffsetMs: a.meanOffsetMs,
    errors: a.errors?.map(classCountJson)
  })

export const loadRunIndexJson = (index: LoadRunIndex): Record<string, unknown> =>
  pruned({
    meta: pruned({
      startedMs: index.meta.startedMs,
      finishedMs: index.meta.finishedMs,
      finished: index.meta.finished,
      target: index.meta.target,
      cps: index.meta.cps,
      durationSecs: index.meta.durationSecs,
      maxInFlight: index.meta.maxInFlight,
      egress: index.meta.egress,
      profile: index.meta.profile,
      commandLine: index.meta.commandLine,
      sampling: index.meta.sampling && {
        sampleCap: index.meta.sampling.sampleCap,
        recordEvery: index.meta.sampling.recordEvery,
        chaseCalls: index.meta.sampling.chaseCalls
      }
    }),
    progress: {
      elapsedMs: index.progress.elapsedMs,
      offeringMs: index.progress.offeringMs,
      targetCps: index.progress.targetCps,
      draining: index.progress.draining,
      offered: index.progress.offered,
      placed: index.progress.placed,
      inFlight: index.progress.inFlight,
      ok: index.progress.ok,
      failed: index.progress.failed,
      rejected: index.progress.rejected,
      generatorFailures: index.progress.generatorFailures,
      retransmissionsReceived: index.progress.retransmissionsReceived,
      retransmissionsResent: index.progress.retransmissionsResent,
      retransmitEngine: index.progress.retransmitEngine
    },
    series: index.series?.map((p) => ({
      startMs: p.startMs,
      spanMs: p.spanMs,
      placed: p.placed,
      ok: p.ok,
      failed: p.failed,
      rejected: p.rejected,
      retransmissionsReceived: p.retransmissionsReceived,
      retransmissionsResent: p.retransmissionsResent,
      p50Ms: p.p50Ms,
      p99Ms: p.p99Ms
    })),
    rateEvents: index.rateEvents?.map((e) => ({ atMs: e.atMs, kind: e.kind, fromCps: e.fromCps, toCps: e.toCps })),
    rateEventsDropped: index.rateEventsDropped,
    recentFailures: index.recentFailures?.map((f) =>
      pruned({ atMs: f.atMs, scenario: f.scenario, class: f.class, case: f.case, page: f.page })
    ),
    recording: index.recording?.map((r) => ({
      scenario: r.scenario,
      placed: r.placed,
      recorded: r.recorded,
      accepted: r.accepted,
      audited: r.audited
    })),
    ladders: index.ladders?.map((l) =>
      pruned({
        scenario: l.scenario,
        calls: l.calls,
        engine: l.engine,
        lanes: l.lanes,
        arrows: l.arrows.map(arrowJson),
        other: l.other && arrowJson(l.other),
        afterCall: l.afterCall?.map(classCountJson),
        beforeReply: l.beforeReply?.map(classCountJson)
      })
    ),
    counts: index.counts.map((row) =>
      pruned({
        scenario: row.scenario,
        class: row.class,
        case: row.case,
        count: row.count,
        ok: row.ok,
        judged: row.judged && {
          failed: row.judged.failed,
          judged: row.judged.judged,
          base: row.judged.base,
          basis: row.judged.basis
        },
        error: row.error
      })
    ),
    latency: index.latency.map((row) => ({
      scenario: row.scenario,
      n: row.n,
      meanMs: row.meanMs,
      p50Ms: row.p50Ms,
      p90Ms: row.p90Ms,
      p99Ms: row.p99Ms,
      maxMs: row.maxMs
    })),
    checkpoints: index.checkpoints?.map((row) => ({
      scenario: row.scenario,
      checkpoint: row.checkpoint,
      n: row.n,
      p50Ms: row.p50Ms,
      p90Ms: row.p90Ms,
      p99Ms: row.p99Ms
    })),
    rtt: index.rtt?.map((row) => ({
      scenario: row.scenario,
      exchange: row.exchange,
      n: row.n,
      p50Ms: row.p50Ms,
      p90Ms: row.p90Ms,
      p99Ms: row.p99Ms
    })),
    checks: index.checks?.map((row) => ({ scenario: row.scenario, passed: row.passed, failed: row.failed })),
    canaries: {
      orphans: index.canaries.orphans,
      shed: index.canaries.shed,
      dropsIn: index.canaries.dropsIn,
      dropsOut: index.canaries.dropsOut,
      inboxDrops: index.canaries.inboxDrops,
      ringingExpected: index.canaries.ringingExpected,
      ringingReceived: index.canaries.ringingReceived
    },
    samples: index.samples?.map((group) => ({
      scenario: group.scenario,
      class: group.class,
      case: group.case,
      pages: group.pages
    }))
  })

export const emitLoadRunIndex = (index: LoadRunIndex): string => formatDeclared(loadRunIndexJson(index))
