/**
 * The HTTP contract. Documents cross the wire as `Unknown`: the pivot schema
 * is owned by the Rust crate, and mirroring it here would create a second
 * contract that could drift from it.
 */
import { Attachment, Verification } from "@sip/contracts"
import { Facets } from "@sip/store"
import { Schema } from "effect"
import { Multipart } from "effect/unstable/http"
import { HttpApi, HttpApiEndpoint, HttpApiGroup, HttpApiSchema, OpenApi } from "effect/unstable/httpapi"
import { RAW_FORMS } from "./flows-trim.ts"
import { SchemaErrors } from "./schema-errors.ts"

/** A 404: the id asked for, and the form the route takes ids in where it states one. */
export class NotFound extends Schema.TaggedError<NotFound>()("Site.NotFound", {
  id: Schema.String,
  accepted: Schema.optional(Schema.String),
  /** What the store's resolver says of the id, where it was asked: the forms it reads, or what the id is. */
  note: Schema.optional(Schema.String)
}) {}

const RootView = Schema.Struct({
  id: Schema.String,
  label: Schema.String,
  path: Schema.String
})

export const ConfigView = Schema.Struct({
  /** The site's name (`site-name.ts`), which the top bar and the browser tab show. */
  name: Schema.String,
  configPath: Schema.String,
  caseRoots: Schema.Array(RootView),
  runRoots: Schema.Array(RootView),
  flowsRoots: Schema.Array(RootView),
  triage: Schema.optional(Schema.Struct({
    registry: Schema.String,
    repoRoot: Schema.String,
    lane: Schema.String,
    database: Schema.optional(Schema.String),
    corpus: Schema.String,
    sessions: Schema.String
  })),
  port: Schema.Number,
  /** The address the server binds. */
  bind: Schema.String,
  /** The origin every link the server prints is built under. */
  publicUrl: Schema.String,
  staticDir: Schema.optional(Schema.String),
  /** The deployment's command lines a view offers for copying; a view hides the ones absent. */
  commands: Schema.Struct({
    triage: Schema.optional(Schema.String),
    request: Schema.optional(Schema.String),
    job: Schema.optional(Schema.String)
  })
})

/**
 * The generator's refusal to cut a case. `excluded.json` crosses verbatim for
 * the same reason a pivot document does: the generator owns that shape, and a
 * second declaration of it here could drift from it.
 */
export const CaseExclusion = Schema.Struct({
  path: Schema.String,
  document: Schema.Unknown
})

export const CaseSummary = Schema.Struct({
  id: Schema.String,
  rootId: Schema.String,
  caseId: Schema.String,
  /** The document's `case.id`: what a run's `pivot.json` names it by. */
  caseKey: Schema.String,
  title: Schema.String,
  family: Schema.String,
  origin: Schema.String,
  variant: Schema.String,
  lanes: Schema.Record(Schema.String, Schema.String),
  path: Schema.String,
  /** Empty for an excluded entry: it has no document to draw, compare or run. */
  variants: Schema.Array(Schema.String),
  excluded: Schema.optionalKey(CaseExclusion)
})

export const CaseDetail = Schema.Struct({
  ...CaseSummary.fields,
  document: Schema.Unknown,
  resources: Schema.Array(Schema.String),
  /**
   * The case's ledger row as the cut WROTE it — the answers the backend mock
   * serves on its call and failure routes, verbatim. `null` where the case dir holds
   * none, which is every case the cut refused and every hand-authored one.
   */
  ledger: Schema.Unknown
})

/** One window of a flows document's legs, at the indices the document gives them. */
export const FlowsLegsWindow = Schema.Struct({
  from: Schema.Number,
  legs: Schema.Array(Schema.Unknown)
})

/**
 * The capture source of a case: its document with the legs left out, and the
 * window the pane opens on. `available: false` is an answer, not an error.
 */
export const CaseSource = Schema.Struct({
  available: Schema.Boolean,
  capture: Schema.optional(Schema.String),
  callGroups: Schema.Array(Schema.Number),
  path: Schema.optional(Schema.String),
  document: Schema.Unknown,
  /** Legs the document holds, however few this answer carries. */
  legs: Schema.Number,
  window: FlowsLegsWindow,
  /** The case's legs past that window, one window per run of them. */
  cited: Schema.Array(FlowsLegsWindow),
  detail: Schema.String
})

export const RunSummary = Schema.Struct({
  id: Schema.String,
  rootId: Schema.String,
  runId: Schema.String,
  /** The `case.id` the bundle's `pivot.json` carries: the case this run replayed. */
  caseKey: Schema.String,
  path: Schema.String,
  legs: Schema.Array(Schema.String),
  verdict: Schema.Unknown
})

export const RunDetail = Schema.Struct({
  ...RunSummary.fields,
  pivot: Schema.Unknown,
  timing: Schema.Unknown,
  runConfig: Schema.Unknown,
  recording: Schema.Record(Schema.String, Schema.String)
})

const TriageTicketView = Schema.Struct({
  /** The Ticket ref the pending record names. */
  ref: Schema.String,
  /** open | closed | wontfix | missing — `@sip/contracts` Triage owns the vocabulary. */
  state: Schema.String,
  /** Why the install has no tracker to read the state off, where it has none. */
  unavailable: Schema.optionalKey(Schema.String)
})

/** One case's outcome inside a run: which call, and how it ended. */
const CaseOutcomeView = Schema.Struct({
  caseId: Schema.String,
  passed: Schema.Boolean,
  /** The step this case died at; null where it passed or named none. */
  failedStep: Schema.NullOr(Schema.String),
  /** The lane verdict this case was blocked on; null where it ran. */
  skipped: Schema.NullOr(Schema.String)
})

/** A case's own last run: the most recent run of any set kind whose results hold it. */
const CaseRunView = Schema.Struct({
  caseId: Schema.String,
  runId: Schema.Number,
  /** full | cases | timeline — the set the run named. */
  setKind: Schema.Literals(["full", "cases", "timeline"]),
  passed: Schema.Boolean,
  /** The lane verdict the case was blocked on in that run; null where it ran. */
  skipped: Schema.NullOr(Schema.String),
  bundlePath: Schema.String
})

/** A scripted party's violation the document does not state: information that fails nothing. */
const TriageSourceFault = Schema.Struct({
  rule: Schema.String,
  rfc: Schema.NullOr(Schema.String),
  /** The finding, led by the party. */
  detail: Schema.String
})

/** What a call failed on: the one line a queue row is picked on. */
const TriageMainError = Schema.Struct({
  /** rfc = a charged RFC finding; died = the replay stopped waiting; failure = any other verdict failure, or a record no rule accepts; bad-body = a body no rule accepts; refused = the cut made no case. */
  kind: Schema.Literals(["rfc", "died", "failure", "bad-body", "refused"]),
  /** The RFC rule id, the failure tag, or the refusal reason; null for a died call. */
  rule: Schema.NullOr(Schema.String),
  /** The citation the finding carries (`RFC 3581 §4`); null where it cites none. */
  rfc: Schema.NullOr(Schema.String),
  step: Schema.NullOr(Schema.String),
  /** What a died call waited for, as `BYE` / `200 to INVITE` / `200`. */
  expected: Schema.NullOr(Schema.String),
  detail: Schema.String,
  /** The call's source fault, beside the SUT's failure; absent where it has none. */
  source: Schema.optionalKey(TriageSourceFault)
})

/**
 * One CALL of the lane's queue: a case of the standing run of its capture, at
 * the case's own last run, or a call the standing cut refused to make a case
 * of. The document's facts are joined for the served page only.
 */
export const TriageCallRow = Schema.Struct({
  capture: Schema.String,
  caseId: Schema.String,
  kind: Schema.Literals(["case", "refused"]),
  /** The call key a request cell names the call by; null where no cut names the case. */
  callKey: Schema.NullOr(Schema.String),
  /** The case's own last run; null for a refused call. */
  runId: Schema.NullOr(Schema.Number),
  /** full | cases | timeline — the set that run named; null for a refused call. */
  setKind: Schema.NullOr(Schema.Literals(["full", "cases", "timeline"])),
  /** The standing cut a refused call belongs to; null for a case. */
  prepId: Schema.NullOr(Schema.Number),
  bundlePath: Schema.NullOr(Schema.String),
  documentHash: Schema.NullOr(Schema.String),
  passed: Schema.Boolean,
  /** The lane verdict the case was blocked on; null where it ran. */
  skipped: Schema.NullOr(Schema.String),
  failedStep: Schema.NullOr(Schema.String),
  rfcGating: Schema.NullOr(Schema.Number),
  /** Every reason a refused call was refused under, the first being `mainError.rule`; [] for a case. */
  reasons: Schema.Array(Schema.String),
  /** The features of the document the case ran; [] for a refused call. */
  features: Schema.Array(Schema.String),
  /** Null where the call passed, was skipped, or was never measured. */
  mainError: Schema.NullOr(TriageMainError),
  /** The body rows no rule accepts, whatever `mainError` names; absent reads as 0. */
  bodyFailing: Schema.optionalKey(Schema.Number),
  /** The case's source fault, passed or failed: information that fails nothing; absent where it has none. */
  sourceFault: Schema.optionalKey(TriageSourceFault),
  /** When the run finished, or when the refusing cut was taken. */
  finishedAt: Schema.NullOr(Schema.String),
  family: Schema.NullOr(Schema.String),
  /** The number the first attempt dialled. */
  dialled: Schema.NullOr(Schema.String),
  /** The final status of the first call's last attempt, as text. */
  outcome: Schema.NullOr(Schema.String),
  legs: Schema.NullOr(Schema.Number),
  attempts: Schema.NullOr(Schema.Number),
  /** The run's bundle survives on disk; false = cleaned, row intact. */
  bundle: Schema.Boolean
})

const TriageFacetCount = Schema.Struct({ value: Schema.String, count: Schema.Number })

/**
 * The chip counts, over the rows the scope, the capture and the excludes keep
 * — never narrowed by the includes, so a count answers "of what is left, how
 * many calls died on X" whatever include is active. An excluded value's own
 * chip counts the calls it hides.
 */
const TriageFacets = Schema.Struct({
  /** The charged RFC rules, with the citation they carry. */
  rules: Schema.Array(Schema.Struct({ ...TriageFacetCount.fields, rfc: Schema.NullOr(Schema.String) })),
  /** Every other failure tag of a case, the interpreter's refusal included. */
  failures: Schema.Array(TriageFacetCount),
  expected: Schema.Array(TriageFacetCount),
  /** A call refused under several reasons counts once under each. */
  refusals: Schema.Array(TriageFacetCount),
  /** The row kinds — refused, failed, passed, skipped — in that order. */
  outcomes: Schema.Array(TriageFacetCount),
  /** The features (reroute, 18x mode, transfer, MRF …), whatever the outcome; a call counts under each it carries. */
  features: Schema.Array(TriageFacetCount),
  /** The source faults' rules, whatever the outcome: information, which no outcome count reads. */
  sources: Schema.Array(TriageFacetCount),
  /** Distinct captures among the rows the scope, the capture filter and the excludes keep. */
  captures: Schema.Number
})

/** A lane the run database holds runs on, and how much of it: the lane picker. */
const TriageLaneView = Schema.Struct({
  lane: Schema.String,
  runs: Schema.Number,
  captures: Schema.Number
})

/** The triage queue. `available: false` is an answer: no triage block is configured. */
export const TriageQueueView = Schema.Struct({
  available: Schema.Boolean,
  detail: Schema.String,
  /** The lane this answer reads. */
  lane: Schema.optionalKey(Schema.String),
  lanes: Schema.Array(TriageLaneView),
  /** red (default) | all | passed | failed | skipped | refused — the scope this answer was served under. */
  scope: Schema.String,
  /** Rows the filter kept, of which `rows` carries at most `limit`. */
  matched: Schema.Number,
  limit: Schema.Number,
  facets: TriageFacets,
  rows: Schema.Array(TriageCallRow)
})

/**
 * One replay of one call as the run index LISTS it: how it ENDED, and nothing
 * more. The verdict itself and the evidence are fetched per cell, because a
 * capture holds one cell per sweep and the page reads one of them.
 */
const TriageStudyCell = Schema.Struct({
  name: Schema.String,
  dir: Schema.String,
  /** The lane this cell ran on, as its directory name states it. */
  lane: Schema.optionalKey(Schema.String),
  /** When this call's evidence was last written. */
  writtenAt: Schema.optionalKey(Schema.String),
  /** `verdict.json`'s own status word; absent where the cell wrote none. */
  status: Schema.optionalKey(Schema.String),
  /** Whether the cell passed, as the run's index states it; absent where the run wrote no index naming it. */
  passed: Schema.optionalKey(Schema.Boolean),
  /** The run's index says the cell carries a source fault: information beside `passed`. */
  sourceFault: Schema.optionalKey(Schema.Boolean),
  /** The step the verdict says the replay died at. */
  failedStep: Schema.optionalKey(Schema.String),
  /** The lane verdict this cell was SKIPPED on; absent where the cell ran. */
  skipped: Schema.optionalKey(Schema.String),
  /** The interpreter refused this case: a refusal where a bundle should be. */
  refused: Schema.Boolean
})

/**
 * One verification line of a cell, a batch or a job (`@sip/contracts`
 * `Verification`) as the pages read it: every optional field present, null
 * where the line states none, each mark a boolean.
 */
export const VerificationLineView = Schema.Struct({
  name: Schema.String,
  source: Schema.String,
  answer: Verification.Answer,
  message: Schema.NullOr(Schema.String),
  /** `step`, `expected`, `seen`, `side` where stated, and the pack's own keys beside them. */
  detail: Schema.NullOr(Schema.Record(Schema.String, Schema.Unknown)),
  informative: Schema.Boolean,
  waived: Schema.Boolean
})

export type VerificationLineView = typeof VerificationLineView.Type

/** `line` as the pages read it. */
export const lineViewOf = (line: Verification.Verification): VerificationLineView => ({
  name: line.name,
  source: line.source,
  answer: line.answer,
  message: line.message ?? null,
  detail: line.detail ?? null,
  informative: line.informative === true,
  waived: line.waived === true
})

const TriageHttpHeader = Schema.Struct({ name: Schema.String, value: Schema.String })

/**
 * One routing exchange as `@sip/viewer-core/http` states it. What the exchange
 * did not have (no destination stated, no answer, no transport failure) is an
 * absent key on the wire; a missing body is null.
 */
const TriageHttpExchange = Schema.Struct({
  ordinal: Schema.Number,
  atMs: Schema.Number,
  dst: Schema.optionalKey(Schema.String),
  method: Schema.String,
  path: Schema.String,
  requestHeaders: Schema.Array(TriageHttpHeader),
  requestBody: Schema.Unknown,
  status: Schema.optionalKey(Schema.Number),
  responseHeaders: Schema.Array(TriageHttpHeader),
  responseBody: Schema.Unknown,
  error: Schema.optionalKey(Schema.String),
  source: Schema.Unknown
})

const TriageHttpExchanges = Schema.Struct({
  exchanges: Schema.Array(TriageHttpExchange),
  warnings: Schema.Array(Schema.String)
})

/** The evidence of ONE cell: everything the run index deliberately leaves behind. */
export const TriageStudyCellDetail = Schema.Struct({
  name: Schema.String,
  dir: Schema.String,
  legs: Schema.Array(Schema.String),
  verdict: Schema.Unknown,
  classification: Schema.Unknown,
  /** The post-run RFC audit: `rfc.json`, verbatim. */
  rfc: Schema.Unknown,
  skipped: Schema.Unknown,
  error: Schema.optionalKey(Schema.String),
  pivot: Schema.Unknown,
  timing: Schema.Unknown,
  runConfig: Schema.Unknown,
  runSpec: Schema.Unknown,
  recording: Schema.Record(Schema.String, Schema.String),
  /** The exchanges this cell's SUT had with the routing service, read off `http.json`; null where the bundle holds none. */
  http: Schema.NullOr(TriageHttpExchanges),
  /** What the interpreter wrote on stderr, where the cell ran and did not pass (`stderr.txt`). */
  stderr: Schema.optionalKey(Schema.String),
  /** The calls the SUT reported terminated (`call.terminated.json`, gzipped where the cell failed); null where none is kept. */
  terminated: Schema.Unknown,
  confrontation: Schema.Array(Schema.Unknown),
  confrontationProblem: Schema.optionalKey(Schema.String),
  /** The Call-IDs this cell minted and the window it drove the SUT in (`sut-join.json`); null where it states none. */
  join: Schema.NullOr(Schema.Struct({ callIds: Schema.Array(Schema.String), from: Schema.String, to: Schema.String })),
  /** The cell's `sut-log` attachments: the SUT's log over its window, titled by its container, its two ends where it is long. */
  sutLogs: Schema.Array(Schema.Struct({ title: Schema.String, text: Schema.String, bytes: Schema.Number })),
  /** The contexts the SUT still held after a batch whose call this cell placed. */
  held: Schema.Array(Schema.Unknown),
  /** The cell's verification lines as the store holds them, and the final status they give it; null where it holds none. */
  verifications: Schema.NullOr(Schema.Struct({ status: Verification.FinalStatus, lines: Schema.Array(VerificationLineView) }))
})

/** One recorded trace of a cell: a file of its bundle holding what went over the wire. */
export const TriageTrace = Schema.Struct({
  /** The file's path inside the cell directory. */
  file: Schema.String,
  bytes: Schema.Number,
  kind: Schema.Literals(["sip", "http", "pcap", "log"])
})

const TriageStudySession = Schema.Struct({
  name: Schema.String,
  dir: Schema.String,
  /** Set where the run is set aside: what made it. A link naming its cell opens it; no default does. */
  aside: Schema.optionalKey(Schema.String),
  /** Set where the run was forced onto a lane lacking these capabilities. */
  forced: Schema.optionalKey(Schema.Array(Schema.String)),
  /** When the run directory was last written: how fresh this run is. */
  writtenAt: Schema.optionalKey(Schema.String),
  campaign: Schema.Unknown,
  cells: Schema.Array(TriageStudyCell),
  /** What each batch's gate read of the SUT's call contexts (`contexts.ndjson`). */
  held: Schema.Array(Schema.Unknown)
})

const TriageStudyCase = Schema.Struct({
  id: Schema.String,
  dir: Schema.String,
  document: Schema.Unknown,
  /** The case's ledger row: the answers the mock serves, as the cut wrote them. */
  ledger: Schema.Unknown
})

/** Everything known about one capture: registry, corpus artifacts, session bundles. */
export const TriageStudyView = Schema.Struct({
  capture: Schema.String,
  /** The one call this study is of; absent for the capture whole. */
  case: Schema.optionalKey(Schema.String),
  /** The standing cut's refusal of that call, where it is one the cut made no case of. */
  refusal: Schema.optionalKey(Schema.Struct({
    caseId: Schema.String,
    reasons: Schema.Array(Schema.String),
    callIds: Schema.Array(Schema.String),
    detail: Schema.String
  })),
  disposition: Schema.String,
  record: Schema.Unknown,
  ticket: Schema.optionalKey(TriageTicketView),
  /** What the run database says this capture stands at on the queue's lane. */
  standing: Schema.optionalKey(Schema.Struct({
    lane: Schema.String,
    runId: Schema.Number,
    /** The prep the standing run read its cases as of; null for a run pinned to none. */
    prepId: Schema.NullOr(Schema.Number),
    bundlePath: Schema.String,
    cases: Schema.Number,
    failed: Schema.Number,
    failedSteps: Schema.Array(Schema.String),
    /** Every case of the run, as itself: what the study's counts line reads. */
    caseResults: Schema.Array(CaseOutcomeView),
    rfcGating: Schema.Number,
    finishedAt: Schema.NullOr(Schema.String),
    lastOkRunId: Schema.NullOr(Schema.Number),
    lastOkBundlePath: Schema.NullOr(Schema.String),
    /** Each case's current run: its latest `@develop` run holding it, of any set kind. */
    caseRuns: Schema.Array(CaseRunView)
  })),
  /** What the cut store says this capture stands cut as: the cases' own reading. */
  cut: Schema.optionalKey(Schema.Struct({
    prepId: Schema.Number,
    cutAt: Schema.String,
    broke: Schema.NullOr(Schema.String),
    cases: Schema.Number
  })),
  corpusDir: Schema.optionalKey(Schema.String),
  /** Where the flows document is and how many legs it holds; absent where none is. Its legs come per window, below. */
  flows: Schema.optionalKey(Schema.Struct({ path: Schema.String, legs: Schema.Number })),
  auto: Schema.Unknown,
  /** The generator's refusal to cut, where it wrote one. */
  exclusions: Schema.optionalKey(Schema.Struct({ path: Schema.String, document: Schema.Unknown })),
  cases: Schema.Array(TriageStudyCase),
  sessionDir: Schema.optionalKey(Schema.String),
  sessions: Schema.Array(TriageStudySession),
  /** Where the absolute paths above are rooted: what makes a path repo-relative. */
  repoRoot: Schema.String
})

/**
 * One rule of the deployment's roster, and what the corpus came to under it.
 * Every rule is a row whether or not anything reached it: a table that lists
 * only what fired cannot show the rule that fires nowhere, which is the one it
 * exists to catch.
 */
const RuleBucket = Schema.Struct({
  /** The subdivision the count is under: a record class, `shape`, `blessed`, `fired`. */
  bucket: Schema.String,
  hits: Schema.Number,
  cells: Schema.Number
})

const RuleLaneClass = Schema.Struct({
  lane: Schema.String,
  /** accepted | known-bug | unlisted — the lane list's own ruling. */
  class: Schema.String,
  ticket: Schema.String
})

const RuleOffTier = Schema.Struct({
  /** Green cells the rule refused. Every one of them is a defect in the rule. */
  fired: Schema.Number,
  silent: Schema.Number,
  unavailable: Schema.Number,
  evidence: Schema.Array(Schema.String)
})

export const RuleRow = Schema.Struct({
  /** refusal | tolerance | bless | declaration. */
  family: Schema.String,
  id: Schema.String,
  /** pre-run | post-run — whether the rule speaks before a run or about one. */
  stage: Schema.String,
  /** capture | case | case-document | run-cell | sweep — the evidence it reads. */
  tier: Schema.String,
  subject: Schema.String,
  disposition: Schema.String,
  /** Why the rule can match nothing; its zero is then its own claim. */
  inert: Schema.optionalKey(Schema.String),
  /** Why no replay can contradict it — about falsification, never about firing. */
  unfalsifiable: Schema.optionalKey(Schema.String),
  lanes: Schema.Array(RuleLaneClass),
  /** measured | declared-zero | unexercised | unrostered. */
  standing: Schema.String,
  hits: Schema.Number,
  cells: Schema.Number,
  buckets: Schema.Array(RuleBucket),
  samples: Schema.Array(Schema.String),
  offTier: Schema.optionalKey(RuleOffTier)
})

/** What every count was taken over. Each number bounds a zero on the page. */
const RulePopulation = Schema.Struct({
  refusalCaptures: Schema.Number,
  refusalRecords: Schema.Number,
  standingRuns: Schema.Number,
  measuredRuns: Schema.Number,
  cutCaptures: Schema.Number,
  cutCases: Schema.Number
})

const RuleOffTierPass = Schema.Struct({
  at: Schema.String,
  captures: Schema.Number,
  evaluated: Schema.Number,
  unreadable: Schema.Number,
  findings: Schema.Number
})

export const RulesView = Schema.Struct({
  available: Schema.Boolean,
  detail: Schema.String,
  lane: Schema.optionalKey(Schema.String),
  lanes: Schema.Array(TriageLaneView),
  population: RulePopulation,
  offTierPass: Schema.optionalKey(RuleOffTierPass),
  rows: Schema.Array(RuleRow)
})

/**
 * A capture dropped on the viewer: its files — one, or the consecutive files
 * of one tap, which the campaign reads as ONE capture — and the lane to
 * replay it on. No lane = added to the replay population and cut, nothing
 * replayed.
 */
export const CaptureUpload = Schema.Struct({
  file: Multipart.FilesSchema,
  lane: Schema.optional(Schema.String)
}).pipe(HttpApiSchema.asMultipart())

/** The ingest job the drop started, and where its capture will be readable. */
export const CaptureIngestStart = Schema.Struct({
  /** The capture name the pipeline will use: the study view's own key. */
  capture: Schema.String,
  started: Schema.Boolean,
  detail: Schema.String,
  /** The page the capture is followed on: its study view, or `/pcaps` for an inbox drop. */
  study: Schema.String,
  /** The drop went to the workspace's inbox, to be anonymised into its pcap root. */
  inbox: Schema.optional(Schema.Boolean),
  /** The job in the store; absent where the source command made none. */
  jobId: Schema.optional(Schema.Number)
})

/** The source command's launch for one capture, as this server saw it; the jobs page reads the job itself. */
export const CaptureIngestState = Schema.Struct({
  /** Never present: an ingest is a job of the store, not a slot of this server. */
  busyWith: Schema.optional(Schema.String),
  last: Schema.optional(Schema.Struct({
    exit: Schema.Number,
    startedAt: Schema.String,
    finishedAt: Schema.String,
    logTail: Schema.String
  })),
  detail: Schema.String
})

/** The drop was not a capture the viewer will take: a name, and why. */
export class CaptureRefused extends Schema.TaggedError<CaptureRefused>()(
  "Site.CaptureRefused",
  { filename: Schema.String, detail: Schema.String }
) {}

/**
 * One lane the deployment runs, the ceilings it clamps a request's knobs to
 * and the knobs it defaults: what the knob fields show beside their
 * inputs.
 */
export const LaneView = Schema.Struct({
  lane: Schema.String,
  /** A box-wide singleton SUT: one replay at a time, the next waits by name. */
  exclusive: Schema.Boolean,
  /** Cells at once the lane tolerates; null = no ceiling of its own. */
  cellCeiling: Schema.NullOr(Schema.Number),
  /** Cell starts per second the lane caps at; null = as fast as slots free. */
  startCeiling: Schema.NullOr(Schema.Number),
  /** What a request on the lane takes where nobody states a knob. */
  defaults: Schema.Struct({
    concurrency: Schema.Number,
    batchSize: Schema.Number,
    startsPerSecond: Schema.Number
  })
})

/** The knobs a request carries; a lane clamps them when it runs. */
export const RequestKnobs = Schema.Struct({
  concurrency: Schema.Number,
  /** Cells per batch. */
  batchSize: Schema.Number,
  memoryMax: Schema.String,
  startsPerSecond: Schema.Number,
  cleanup: Schema.Boolean
})

export class CatalogGroup extends HttpApiGroup.make("catalog")
  .add(HttpApiEndpoint.get("config", "/config", { success: ConfigView })
    .annotate(OpenApi.Description, "This server's configuration: case roots, run roots, the triage block, the commands."))
  .add(HttpApiEndpoint.get("lanes", "/lanes", {
    success: Schema.Struct({ lanes: Schema.Array(LaneView), knobs: RequestKnobs })
  })
    .annotate(OpenApi.Description, "The lanes a request can name, and the default request knobs."))
  .add(HttpApiEndpoint.get("cases", "/cases", { success: Schema.Array(CaseSummary) })
    .annotate(OpenApi.Description, "Every scenario case under the case roots."))
  .add(HttpApiEndpoint.get("case", "/cases/:id", {
    params: { id: Schema.String },
    query: { variant: Schema.optional(Schema.String) },
    success: CaseDetail,
    error: NotFound.pipe(HttpApiSchema.status(404))
  })
    .annotate(OpenApi.Description, "One scenario case's pivot document; `variant` picks a variant file."))
  .add(HttpApiEndpoint.get("caseSource", "/cases/:id/source", {
    params: { id: Schema.String },
    query: { variant: Schema.optional(Schema.String) },
    success: CaseSource,
    error: NotFound.pipe(HttpApiSchema.status(404))
  })
    .annotate(OpenApi.Description, "The capture a scenario case was cut from: its flows document, legs left out, and the case's legs."))
  .add(HttpApiEndpoint.get("caseSourceLegs", "/cases/:id/source/legs", {
    params: { id: Schema.String },
    query: {
      variant: Schema.optional(Schema.String),
      /** The window's first leg, as the document indexes its legs; absent = the first. */
      from: Schema.optional(Schema.FiniteFromString),
      /** Legs asked for; the server serves at most its own cap. */
      count: Schema.optional(Schema.FiniteFromString)
    },
    success: FlowsLegsWindow,
    error: NotFound.pipe(HttpApiSchema.status(404))
  })
    .annotate(OpenApi.Description, "A window of the legs of a scenario case's capture (`from`, `count`)."))
  .add(HttpApiEndpoint.get("runs", "/runs", { success: Schema.Array(RunSummary) })
    .annotate(OpenApi.Description, "Every run recording under the run roots."))
  .add(HttpApiEndpoint.get("run", "/runs/:id", {
    params: { id: Schema.String },
    success: RunDetail,
    error: NotFound.pipe(HttpApiSchema.status(404))
  })
    .annotate(OpenApi.Description, "One run recording: its bundle."))
  .prefix("/api")
{}

/**
 * A capture's flows document with its legs left out, and how many it holds:
 * what a ladder reads once before asking for the legs it draws.
 */
export const TriageFlowsEnvelope = Schema.Struct({
  path: Schema.String,
  legs: Schema.Number,
  document: Schema.Unknown
})

/** One run of one call, on any lane: how the call ended in it. */
const TriageCallHistoryEntry = Schema.Struct({
  lane: Schema.String,
  runId: Schema.Number,
  setKind: Schema.String,
  jobId: Schema.NullOr(Schema.Number),
  finishedAt: Schema.NullOr(Schema.String),
  passed: Schema.Boolean,
  skipped: Schema.NullOr(Schema.String),
  failedStep: Schema.NullOr(Schema.String),
  errorRule: Schema.NullOr(Schema.String),
  errorExpected: Schema.NullOr(Schema.String),
  features: Schema.Array(Schema.String)
})

/** A call's latest run on each lane a request can name, and its recent runs across every lane. */
export const TriageCallHistory = Schema.Struct({
  /** One per lane that ran the call, in `GET /api/lanes` order. */
  latest: Schema.Array(TriageCallHistoryEntry),
  /** Newest first, whatever the lane. */
  recent: Schema.Array(TriageCallHistoryEntry)
})

const DeltaSampleView = Schema.Struct({
  capture: Schema.String,
  caseId: Schema.String,
  session: Schema.String,
  cell: Schema.String,
  step: Schema.String,
  captured: Schema.Array(Schema.String),
  replayed: Schema.Array(Schema.String)
})

/** One (signature, class, rule) of a queue slice's confrontation records, with a few distinct samples. */
const DeltaLineView = Schema.Struct({
  kind: Schema.String,
  signature: Schema.String,
  klass: Schema.String,
  rule: Schema.String,
  rows: Schema.Number,
  cells: Schema.Number,
  samples: Schema.Array(DeltaSampleView)
})

/** The delta sheet of a queue slice. `available: false` is an answer: no triage block, or no store. */
export const DeltaSheetView = Schema.Struct({
  available: Schema.Boolean,
  detail: Schema.String,
  lane: Schema.String,
  scope: Schema.String,
  matched: Schema.Number,
  read: Schema.Number,
  unread: Schema.Number,
  undecodable: Schema.Number,
  elapsedMs: Schema.Number,
  lines: Schema.Array(DeltaLineView)
})

export class TriageGroup extends HttpApiGroup.make("triage")
  .add(HttpApiEndpoint.get("triageQueue", "/triage", {
    query: {
      /** Which SUT's runs to read; absent = the configured lane. */
      lane: Schema.optional(Schema.String),
      /** red (default) | all | passed | failed | skipped | refused. */
      scope: Schema.optional(Schema.String),
      /** Capture-name substring, matched case-insensitively. */
      q: Schema.optional(Schema.String),
      /** Exact main-error rule of a case row: an RFC rule id or a failure tag alike. */
      rule: Schema.optional(Schema.String),
      /** Exact message a died call waited for (`200 to INVITE`). */
      expected: Schema.optional(Schema.String),
      /** Exact refusal reason of a refused call. */
      refusal: Schema.optional(Schema.String),
      /** Exact outcome of a row; any other word is refused. */
      outcome: Schema.optional(Schema.Literals(Facets.OUTCOMES)),
      /** One feature the call's document carries. */
      feature: Schema.optional(Schema.String),
      /** The rule of the source fault a case carries, passed or failed: information, never an outcome. */
      source: Schema.optional(Schema.String),
      /** The values hidden per group; the key repeats per value (`not-refusal=a&not-refusal=b`). */
      "not-rule": Schema.optional(Schema.Array(Schema.String)),
      "not-expected": Schema.optional(Schema.Array(Schema.String)),
      "not-refusal": Schema.optional(Schema.Array(Schema.String)),
      "not-outcome": Schema.optional(Schema.Array(Schema.Literals(Facets.OUTCOMES))),
      "not-feature": Schema.optional(Schema.Array(Schema.String)),
      "not-source": Schema.optional(Schema.Array(Schema.String)),
      /** Rows to serve after filtering; the rest are counted, not sent. */
      limit: Schema.optional(Schema.FiniteFromString)
    },
    success: TriageQueueView
  })
    .annotate(OpenApi.Description, "The triage queue: one row per call of the captures, filtered by the queue's words."))
  .add(HttpApiEndpoint.get("triageStudy", "/triage/:capture", {
    params: { capture: Schema.String },
    query: {
      lane: Schema.optional(Schema.String),
      /** One call of the capture: the study is of it alone. */
      case: Schema.optional(Schema.String)
    },
    success: TriageStudyView,
    error: NotFound.pipe(HttpApiSchema.status(404))
  })
    .annotate(OpenApi.Description, "One capture's study on a lane: its cut, cases, runs and cells; `case` narrows it to one call."))
  .add(HttpApiEndpoint.get("triageFlowsEnvelope", "/triage/:capture/flows/envelope", {
    params: { capture: Schema.String },
    success: TriageFlowsEnvelope,
    error: NotFound.pipe(HttpApiSchema.status(404))
  })
    .annotate(OpenApi.Description, "The capture's flows document with its legs left out, and how many it holds."))
  .add(HttpApiEndpoint.get("triageFlowsLegs", "/triage/:capture/flows/legs", {
    params: { capture: Schema.String },
    query: {
      /** The window's first leg, as the document indexes its legs; absent = the first. */
      from: Schema.optional(Schema.FiniteFromString),
      /** Legs asked for; the server serves at most its own cap. */
      count: Schema.optional(Schema.FiniteFromString),
      /** Each message's `raw`: `full` (default), `head` (start line and headers), `none`; `line`: each message in one line. */
      raw: Schema.optional(Schema.Literals(RAW_FORMS))
    },
    success: FlowsLegsWindow,
    error: NotFound.pipe(HttpApiSchema.status(404))
  })
    .annotate(OpenApi.Description, "A window of the capture's flows legs (`from`, `count`); `raw=head`/`none` drops bodies, `raw=line` cuts each message to one line."))
  .add(HttpApiEndpoint.get("triageCallHistory", "/triage/:capture/history", {
    params: { capture: Schema.String },
    query: {
      case: Schema.String,
      /** Runs to serve, newest first; absent = `CALL_HISTORY_LIMIT`. */
      limit: Schema.optional(Schema.FiniteFromString)
    },
    success: TriageCallHistory
  })
    .annotate(OpenApi.Description, "One call's latest run per lane, and its recent runs across every lane."))
  .add(HttpApiEndpoint.get("triageCaseDocument", "/triage/:capture/document", {
    params: { capture: Schema.String },
    query: { case: Schema.String },
    success: Schema.Unknown,
    error: NotFound.pipe(HttpApiSchema.status(404))
  })
    .annotate(OpenApi.Description, "One call's cut: the pivot document the standing cut made of it (`case`)."))
  .add(HttpApiEndpoint.get("triageCell", "/triage/:capture/cells/:session/:cell", {
    params: { capture: Schema.String, session: Schema.String, cell: Schema.String },
    success: TriageStudyCellDetail,
    error: NotFound.pipe(HttpApiSchema.status(404))
  })
    .annotate(OpenApi.Description, "One cell's evidence: verdict, RFC audit, pivot, recording, HTTP exchanges, SUT records."))
  .add(HttpApiEndpoint.get("triageRunText", "/triage/:capture/runs/:run/text", {
    params: { capture: Schema.String, run: Schema.String },
    query: {
      /** Which lane's standing the run is read under; absent = the configured one. */
      lane: Schema.optional(Schema.String),
      /** `1` unfolds every payload: SIP bodies and HTTP exchanges inline. */
      full: Schema.optional(Schema.String),
      /** One call of the capture: the run is exported over that call's cells alone. */
      case: Schema.optional(Schema.String),
      /** One cell of the run: the export is that cell alone, its traces over the cell's window. */
      cell: Schema.optional(Schema.String)
    },
    success: Schema.String.pipe(HttpApiSchema.asText()),
    error: NotFound.pipe(HttpApiSchema.status(404))
  })
    .annotate(OpenApi.Description, "A run as plain text: verdicts, capture ladder, replay ladders, the cells' SUT logs and trace logs; `case` or `cell` narrows it."))
  .add(HttpApiEndpoint.get("triageCellDigest", "/triage/:capture/runs/:run/cells/:cell/digest", {
    params: { capture: Schema.String, run: Schema.String, cell: Schema.String },
    /** The lane the cell ran on, where its name does not say. */
    query: { lane: Schema.optional(Schema.String) },
    success: Schema.String.pipe(HttpApiSchema.asText({ contentType: "text/markdown" })),
    error: NotFound.pipe(HttpApiSchema.status(404))
  })
    .annotate(
      OpenApi.Description,
      "One cell in a page: the gate that decided it, failures, RFC findings on their ladder messages, lane facts, routing exchanges, the SUT's log lines of the call; JSON with `Accept: application/json`."
    ))
  .add(HttpApiEndpoint.get("triageCellMessage", "/triage/:capture/runs/:run/cells/:cell/message", {
    params: { capture: Schema.String, run: Schema.String, cell: Schema.String },
    query: {
      /** The ladder number of the message (`m58` is 58). */
      m: Schema.optional(Schema.FiniteFromString),
      /** A flow step id: every message at that step, the step itself, the captured message it cites. */
      step: Schema.optional(Schema.String)
    },
    success: Schema.String.pipe(HttpApiSchema.asText()),
    error: NotFound.pipe(HttpApiSchema.status(404))
  })
    .annotate(
      OpenApi.Description,
      "One replayed message with its headers and body, by ladder number (`m`) or step id (`step`), with the case step and the captured message it cites."
    ))
  .add(HttpApiEndpoint.get("triageCellTraces", "/triage/:capture/runs/:run/cells/:cell/traces", {
    params: { capture: Schema.String, run: Schema.String, cell: Schema.String },
    success: Schema.Array(TriageTrace),
    error: NotFound.pipe(HttpApiSchema.status(404))
  })
    .annotate(OpenApi.Description, "The recorded traces of one cell: per-leg SIP, HTTP exchanges, pcaps, kept logs."))
  .add(HttpApiEndpoint.get("triageCellTrace", "/triage/:capture/runs/:run/cells/:cell/trace", {
    params: { capture: Schema.String, run: Schema.String, cell: Schema.String },
    /** The trace's own name, as the listing states it; a name it does not hold is a 404. */
    query: { file: Schema.String },
    success: Schema.Uint8Array.pipe(HttpApiSchema.asUint8Array()),
    error: NotFound.pipe(HttpApiSchema.status(404))
  })
    .annotate(OpenApi.Description, "One recorded trace's bytes, by the name the listing gave it (`file`)."))
  .prefix("/api")
{}

/** The delta sheet of a queue slice: every kept call's confrontation records, folded. */
export class DeltasGroup extends HttpApiGroup.make("deltas")
  .add(HttpApiEndpoint.get("deltaSheet", "/deltas", {
    /** The queue's own filter words; there is no row cap, every kept call is read. */
    query: {
      lane: Schema.optional(Schema.String),
      scope: Schema.optional(Schema.String),
      q: Schema.optional(Schema.String),
      rule: Schema.optional(Schema.String),
      expected: Schema.optional(Schema.String),
      refusal: Schema.optional(Schema.String),
      outcome: Schema.optional(Schema.Literals(Facets.OUTCOMES)),
      feature: Schema.optional(Schema.String),
      source: Schema.optional(Schema.String),
      "not-rule": Schema.optional(Schema.Array(Schema.String)),
      "not-expected": Schema.optional(Schema.Array(Schema.String)),
      "not-refusal": Schema.optional(Schema.Array(Schema.String)),
      "not-outcome": Schema.optional(Schema.Array(Schema.Literals(Facets.OUTCOMES))),
      "not-feature": Schema.optional(Schema.Array(Schema.String)),
      "not-source": Schema.optional(Schema.Array(Schema.String))
    },
    success: DeltaSheetView
  })
    .annotate(OpenApi.Description, "The delta sheet of a queue slice: confrontation records folded by signature, with samples."))
  .prefix("/api")
{}

/**
 * Captures entering the population: the drop endpoint and its job's standing.
 * Its own group because it WRITES — every other read-only group answers about
 * what the pipeline already produced.
 */
export class CapturesGroup extends HttpApiGroup.make("captures")
  .add(HttpApiEndpoint.post("captureIngest", "/captures", {
    payload: CaptureUpload,
    success: CaptureIngestStart,
    error: CaptureRefused.pipe(HttpApiSchema.status(400))
  })
    .annotate(OpenApi.Description, "Adds captures to the population (multipart upload) and starts their ingest."))
  .add(HttpApiEndpoint.get("captureIngestState", "/captures/:capture/ingest", {
    params: { capture: Schema.String },
    success: CaptureIngestState
  })
    .annotate(OpenApi.Description, "One capture's ingest standing."))
  .prefix("/api")
{}

/** The global rules view: the roster, joined to what the corpus came to. */
export class RulesGroup extends HttpApiGroup.make("rules")
  .add(HttpApiEndpoint.get("rules", "/rules", {
    /** Which SUT's runs the post-run counts read; absent = the configured lane. */
    query: { lane: Schema.optional(Schema.String) },
    success: RulesView
  })
    .annotate(OpenApi.Description, "The rule roster, with what the corpus came to under each rule on a lane."))
  .prefix("/api")
{}


/**
 * A selection as the page states it, ONE form: the filter bar's
 * words, captures meant whole, the universe with its predicates, or cells —
 * the ticked rows, each by call key or by the case id the cut named it.
 */
export const RequestSelection = Schema.Union([
  Schema.Struct({
    kind: Schema.Literal("filter"),
    /** The lane whose queue the bar's words are read on; absent, each target lane's own. */
    lane: Schema.optional(Schema.String),
    scope: Schema.optional(Schema.String),
    q: Schema.optional(Schema.String),
    rule: Schema.optional(Schema.String),
    expected: Schema.optional(Schema.String),
    refusal: Schema.optional(Schema.String),
    outcome: Schema.optional(Schema.String),
    feature: Schema.optional(Schema.String),
    source: Schema.optional(Schema.String),
    "not-rule": Schema.optional(Schema.Array(Schema.String)),
    "not-expected": Schema.optional(Schema.Array(Schema.String)),
    "not-refusal": Schema.optional(Schema.Array(Schema.String)),
    "not-outcome": Schema.optional(Schema.Array(Schema.String)),
    "not-feature": Schema.optional(Schema.Array(Schema.String)),
    "not-source": Schema.optional(Schema.Array(Schema.String))
  }),
  Schema.Struct({ kind: Schema.Literal("captures"), captures: Schema.Array(Schema.String) }),
  Schema.Struct({
    kind: Schema.Literal("universe"),
    universe: Schema.Literals(["all", "standing"]),
    maxSpan: Schema.optional(Schema.Number),
    greenOn: Schema.optional(Schema.String),
    sample: Schema.optional(Schema.Number),
    limit: Schema.optional(Schema.Number)
  }),
  Schema.Struct({
    kind: Schema.Literal("cells"),
    cells: Schema.Array(Schema.Struct({
      capture: Schema.String,
      callKey: Schema.optional(Schema.String),
      /** The case id, resolved to its key off the standing cut where no key is given. */
      caseId: Schema.optional(Schema.String)
    }))
  })
])

/** What to preview: the selection on each lane, widened to `wholeFirst` whole. */
export const RequestPreviewRequest = Schema.Struct({
  lanes: Schema.Array(Schema.String),
  selection: RequestSelection,
  wholeFirst: Schema.optional(Schema.Array(Schema.String))
})

/** One capture of a preview on one lane. */
export const RequestPreviewCapture = Schema.Struct({
  capture: Schema.String,
  cells: Schema.Number,
  /** The keys its cut holds at the prep: what the capture comes to whole, once widened to. */
  cut: Schema.Number,
  /** full = every key of its cut at the prep; cases otherwise. */
  kind: Schema.Literals(["full", "cases"])
})

/** The selection resolved on one lane, read-only: what `Requests.resolve` would derive, before any row exists. */
export const RequestLanePreview = Schema.Struct({
  lane: Schema.String,
  cells: Schema.Number,
  captures: Schema.Array(RequestPreviewCapture),
  /** The captures the selection means whole. */
  whole: Schema.Array(Schema.String),
  /** Rows the filter matched that no cut names a key for: on no request. */
  unkeyed: Schema.Number
})

export const RequestPreview = Schema.Struct({
  prepId: Schema.Number,
  lanes: Schema.Array(RequestLanePreview)
})

/** What to create: one request per lane, each frozen on its own lane. */
export const RequestCreateRequest = Schema.Struct({
  lanes: Schema.Array(Schema.String),
  selection: RequestSelection,
  wholeFirst: Schema.optional(Schema.Array(Schema.String)),
  /** Absent knobs take each lane's defaults `GET /api/lanes` states. */
  knobs: Schema.optional(Schema.Struct({
    concurrency: Schema.optional(Schema.Number),
    batchSize: Schema.optional(Schema.Number),
    memoryMax: Schema.optional(Schema.String),
    startsPerSecond: Schema.optional(Schema.Number),
    cleanup: Schema.optional(Schema.Boolean)
  }))
})

export const RequestCreated = Schema.Struct({
  lane: Schema.String,
  requestId: Schema.Number,
  cells: Schema.Number,
  captures: Schema.Array(Schema.String)
})

/** The selection cannot be frozen: no cut, a capture no cut holds, nothing named. */
export class RequestRefused extends Schema.TaggedError<RequestRefused>()("Site.RequestRefused", {
  detail: Schema.String
}) {}

/** The request builder: a preview, then the rows. */
export class RequestsGroup extends HttpApiGroup.make("requests")
  .add(HttpApiEndpoint.post("requestPreview", "/requests/preview", {
    payload: RequestPreviewRequest,
    success: RequestPreview,
    error: RequestRefused.pipe(HttpApiSchema.status(400))
  })
    .annotate(OpenApi.Description, "What a request would select and run, without creating it."))
  .add(HttpApiEndpoint.post("requestCreate", "/requests", {
    payload: RequestCreateRequest,
    success: Schema.Struct({ created: Schema.Array(RequestCreated) }),
    error: RequestRefused.pipe(HttpApiSchema.status(400))
  })
    .annotate(OpenApi.Description, "Creates replay requests from a selection."))
  .prefix("/api")
{}

/**
 * A job to start, detached, as the job command's `start` starts it. A replay
 * with `recutFirst` is TWO jobs: a cut over the request's captures, then the
 * replay waiting for its end. An ingest names paths under the source roots. A
 * pcap replay is TWO jobs too: an ingest + cut of a file of the pcap root,
 * then a replay of its capture waiting for it.
 */
export const JobStartRequest = Schema.Union([
  Schema.Struct({
    kind: Schema.Literal("replay"),
    requestId: Schema.Number,
    knobs: Schema.optional(Schema.Struct({
      concurrency: Schema.optional(Schema.Number),
      batch: Schema.optional(Schema.Number),
      memoryMax: Schema.optional(Schema.String),
      startsPerSecond: Schema.optional(Schema.Number),
      cleanup: Schema.optional(Schema.Boolean),
      /** Seconds to wait for a held lane or box; absent = the default patience. */
      wait: Schema.optional(Schema.Number),
      /** The collection level the replay takes; absent = the pack's `capture` level (`DeploymentConfig.CAPTURE_LEVEL`). */
      collection: Schema.optional(Schema.String)
    })),
    recutFirst: Schema.optional(Schema.Literals(["stale-only", "force"]))
  }),
  Schema.Struct({ kind: Schema.Literal("retire"), captures: Schema.Array(Schema.String) }),
  Schema.Struct({
    kind: Schema.Literal("campaign"),
    capture: Schema.String,
    /** The lanes, by the name the campaign page shows (`<prefix><short>`) or the short one (`<short>`). */
    lanes: Schema.Array(Schema.String),
    /** One case, by its test id, and one of its sub-cases; absent, every case. */
    case: Schema.optional(Schema.String),
    subcase: Schema.optional(Schema.String),
    /** Rebuild the system's data from scratch before the run. */
    rebuild: Schema.optional(Schema.Boolean),
    /** Campaign tags the run takes on the lanes that exclude them. */
    allow: Schema.optional(Schema.Array(Schema.String)),
    /** The collection level the run takes; absent = the workspace's default. */
    collection: Schema.optional(Schema.String)
  }),
  Schema.Struct({
    kind: Schema.Literal("ingest"),
    /** Under the source roots, as typed; a relative path resolves against the repo. */
    paths: Schema.Array(Schema.String),
    /** The paths are the consecutive files of ONE capture. */
    set: Schema.optional(Schema.Boolean),
    /** Cut what is stale once the ingest has landed. */
    cut: Schema.optional(Schema.Boolean)
  }),
  Schema.Struct({
    kind: Schema.Literal("pcap-replay"),
    /** A file name of the workspace's pcap root. */
    file: Schema.String,
    /** The lane, by its catalog id. */
    lane: Schema.String,
    /** Drive a lane a capture replay does not fit; its runs are recorded forced. */
    force: Schema.optional(Schema.Boolean)
  })
])

export const JobStarted = Schema.Struct({
  id: Schema.Number,
  kind: Schema.String,
  /** One line naming the work, as the jobs list shows it. */
  label: Schema.String,
  /** Where its log is written. */
  logPath: Schema.String
})

/** A job as the list shows it: the row's facts the page reads, and its units landed over total, failed, gone. */
export const JobListed = Schema.Struct({
  id: Schema.Number,
  kind: Schema.String,
  status: Schema.String,
  label: Schema.String,
  lane: Schema.NullOr(Schema.String),
  requestId: Schema.NullOr(Schema.Number),
  createdAt: Schema.String,
  startedAt: Schema.NullOr(Schema.String),
  finishedAt: Schema.NullOr(Schema.String),
  exit: Schema.NullOr(Schema.Number),
  /** The lock holder's line while the job waits for it. */
  waitingOn: Schema.NullOr(Schema.String),
  batchAt: Schema.NullOr(Schema.Number),
  batches: Schema.NullOr(Schema.Number),
  stopRequestedAt: Schema.NullOr(Schema.String),
  restartOf: Schema.NullOr(Schema.Number),
  /** `resume` | `again` for a restart; null for a first run. */
  mode: Schema.NullOr(Schema.Literals(["resume", "again"])),
  landed: Schema.Number,
  total: Schema.Number,
  failed: Schema.Number,
  gone: Schema.Number
})

/** One page of the queue, counted: running rows first, then queued, then the ended newest first. */
export const JobsPage = Schema.Struct({
  rows: Schema.Array(JobListed),
  total: Schema.Number,
  limit: Schema.Number,
  offset: Schema.Number
})

/** The progress query's counts: cells for a replay, captures or runs for the other kinds. */
export const JobProgress = Schema.Struct({
  total: Schema.Number,
  landed: Schema.Number,
  pending: Schema.Number,
  goneRefused: Schema.Number,
  goneMissing: Schema.Number,
  passed: Schema.Number,
  failed: Schema.Number,
  skipped: Schema.Number,
  runs: Schema.Number,
  openRuns: Schema.Number
})

const JobKin = Schema.Struct({
  id: Schema.Number,
  status: Schema.String,
  mode: Schema.NullOr(Schema.Literals(["resume", "again"]))
})

/** One job: its row, its progress, the request it runs and the restart chain around it. */
export const JobDetail = Schema.Struct({
  id: Schema.Number,
  kind: Schema.String,
  status: Schema.String,
  label: Schema.String,
  lane: Schema.NullOr(Schema.String),
  createdAt: Schema.String,
  createdBy: Schema.String,
  pid: Schema.NullOr(Schema.Number),
  startedAt: Schema.NullOr(Schema.String),
  finishedAt: Schema.NullOr(Schema.String),
  exit: Schema.NullOr(Schema.Number),
  logPath: Schema.NullOr(Schema.String),
  waitingOn: Schema.NullOr(Schema.String),
  sweepId: Schema.NullOr(Schema.Number),
  batchAt: Schema.NullOr(Schema.Number),
  batches: Schema.NullOr(Schema.Number),
  stopRequestedAt: Schema.NullOr(Schema.String),
  stopGrace: Schema.NullOr(Schema.Number),
  restartOf: Schema.NullOr(Schema.Number),
  mode: Schema.NullOr(Schema.Literals(["resume", "again"])),
  planned: Schema.NullOr(Schema.Number),
  args: Schema.Record(Schema.String, Schema.Unknown),
  progress: JobProgress,
  request: Schema.NullOr(Schema.Struct({
    id: Schema.Number,
    lane: Schema.String,
    cells: Schema.Number,
    captures: Schema.Number,
    /** The selection as the request command's `create` would state it. */
    selection: Schema.String
  })),
  /** What this job restarts, nearest first, and what restarted it. */
  lineage: Schema.Struct({ ancestors: Schema.Array(JobKin), descendants: Schema.Array(JobKin) }),
  /** The job's own verification lines. */
  lines: Schema.Array(VerificationLineView),
  /** The job's batches in the order they started, each with the lines its end gates answered. */
  batchList: Schema.Array(Schema.Struct({
    id: Schema.Number,
    lane: Schema.String,
    attempt: Schema.Number,
    cells: Schema.Number,
    startedAt: Schema.String,
    finishedAt: Schema.NullOr(Schema.String),
    detail: Schema.NullOr(Schema.String),
    lines: Schema.Array(VerificationLineView)
  }))
})

/** A byte range of the job's log: the next poll starts at `to`; `done` once the job has ended and the end was reached. */
export const JobLogRange = Schema.Struct({
  from: Schema.Number,
  to: Schema.Number,
  text: Schema.String,
  done: Schema.Boolean
})

export const JobGoneCell = Schema.Struct({
  capture: Schema.String,
  callKey: Schema.String,
  outcome: Schema.String,
  reasons: Schema.Array(Schema.String)
})

export const JobRedCell = Schema.Struct({
  capture: Schema.String,
  lane: Schema.String,
  caseId: Schema.String,
  runId: Schema.Number,
  error: Schema.NullOr(TriageMainError),
  /** The body rows no rule accepts, whatever `error` names. */
  bodyFailing: Schema.Number
})

/** A slice of the job's cells of one kind: the gone ones with their reasons, or the ones that landed red so far. */
export const JobCells = Schema.Union([
  Schema.Struct({ outcome: Schema.Literal("gone"), cells: Schema.Array(JobGoneCell), total: Schema.Number }),
  Schema.Struct({ outcome: Schema.Literal("red"), cells: Schema.Array(JobRedCell), total: Schema.Number })
])

/** What deleting the job's runs takes: the page confirms once on these counts. */
export const JobDeletePlan = Schema.Struct({
  runs: Schema.Number,
  rows: Schema.Struct({ runs: Schema.Number, caseResults: Schema.Number, runRuleHits: Schema.Number }),
  bundles: Schema.Number,
  held: Schema.Array(Schema.Struct({ id: Schema.Number, why: Schema.String }))
})

/** Nothing was launched, and why: no such request, paths outside the roots, no census for a cut. */
export class JobRefused extends Schema.TaggedError<JobRefused>()("Site.JobRefused", {
  detail: Schema.String
}) {}

const JobParams = { id: Schema.NumberFromString }

export class JobsGroup extends HttpApiGroup.make("jobs")
  .add(HttpApiEndpoint.post("jobStart", "/jobs", {
    payload: JobStartRequest,
    /** In launch order: the cut before the replay it was queued ahead of. */
    success: Schema.Struct({ jobs: Schema.Array(JobStarted) }),
    error: JobRefused.pipe(HttpApiSchema.status(400))
  })
    .annotate(OpenApi.Description, "Starts the jobs of a request."))
  .add(HttpApiEndpoint.get("jobList", "/jobs", {
    query: {
      /** One status, `active` (running and queued), `ended`, or absent for every row. */
      status: Schema.optional(Schema.String),
      limit: Schema.optional(Schema.FiniteFromString),
      offset: Schema.optional(Schema.FiniteFromString)
    },
    success: JobsPage
  })
    .annotate(OpenApi.Description, "The jobs, newest first, paged (`status`, `limit`, `offset`)."))
  .add(HttpApiEndpoint.get("jobShow", "/jobs/:id", {
    params: JobParams,
    success: JobDetail,
    error: NotFound.pipe(HttpApiSchema.status(404))
  })
    .annotate(OpenApi.Description, "One job: its request, progress and lineage."))
  .add(HttpApiEndpoint.get("jobLog", "/jobs/:id/log", {
    params: JobParams,
    query: { from: Schema.optional(Schema.FiniteFromString) },
    success: JobLogRange,
    error: NotFound.pipe(HttpApiSchema.status(404))
  })
    .annotate(OpenApi.Description, "One job's log from offset `from`."))
  .add(HttpApiEndpoint.get("jobCells", "/jobs/:id/cells", {
    params: JobParams,
    query: { outcome: Schema.Literals(["gone", "red"]), limit: Schema.optional(Schema.FiniteFromString) },
    success: JobCells,
    error: NotFound.pipe(HttpApiSchema.status(404))
  })
    .annotate(OpenApi.Description, "One job's red or lost cells (`outcome`)."))
  .add(HttpApiEndpoint.post("jobStop", "/jobs/:id/stop", {
    params: JobParams,
    payload: Schema.Struct({ grace: Schema.optional(Schema.Number) }),
    success: Schema.Struct({ stopped: Schema.Number }),
    error: [NotFound.pipe(HttpApiSchema.status(404)), JobRefused.pipe(HttpApiSchema.status(400))]
  })
    .annotate(OpenApi.Description, "Stops a running job."))
  .add(HttpApiEndpoint.post("jobRestart", "/jobs/:id/restart", {
    params: JobParams,
    payload: Schema.Struct({ mode: Schema.Literals(["resume", "again"]) }),
    success: JobStarted,
    error: [NotFound.pipe(HttpApiSchema.status(404)), JobRefused.pipe(HttpApiSchema.status(400))]
  })
    .annotate(OpenApi.Description, "Restarts a job: resumes it, or runs it again."))
  .add(HttpApiEndpoint.get("jobDeletePlan", "/jobs/:id/delete-plan", {
    params: JobParams,
    success: JobDeletePlan,
    error: NotFound.pipe(HttpApiSchema.status(404))
  })
    .annotate(OpenApi.Description, "What deleting a job would remove."))
  .add(HttpApiEndpoint.post("jobDelete", "/jobs/:id/delete", {
    params: JobParams,
    success: JobStarted,
    error: [NotFound.pipe(HttpApiSchema.status(404)), JobRefused.pipe(HttpApiSchema.status(400))]
  })
    .annotate(OpenApi.Description, "Deletes a job's rows and bundles."))
  .prefix("/api")
{}

/** A (capture, lane)'s standing as the source list carries it: the latest full `@develop` run. */
const SourceStanding = Schema.Struct({
  lane: Schema.String,
  runId: Schema.Number,
  status: Schema.String,
  cases: Schema.Number,
  failed: Schema.Number,
  skipped: Schema.Number,
  finishedAt: Schema.NullOr(Schema.String)
})

/** One capture of the source list, as `Sources.list` states it. */
export const SourceRow = Schema.Struct({
  capture: Schema.String,
  sourcePath: Schema.NullOr(Schema.String),
  addedAt: Schema.String,
  ingestedAt: Schema.NullOr(Schema.String),
  /** current | stale | failed. */
  ingest: Schema.String,
  ingestFailed: Schema.NullOr(Schema.String),
  prepId: Schema.NullOr(Schema.Number),
  cutAt: Schema.NullOr(Schema.String),
  /** current | stale | broke | never. */
  cut: Schema.String,
  cutBroke: Schema.NullOr(Schema.String),
  cases: Schema.Number,
  refusals: Schema.Number,
  /** The longest call of the standing cut, in seconds; null where no case states a span. */
  spanSeconds: Schema.NullOr(Schema.Number),
  standings: Schema.Array(SourceStanding)
})

/** A page of the source list, counted. */
export const SourcesPage = Schema.Struct({
  rows: Schema.Array(SourceRow),
  total: Schema.Number,
  limit: Schema.Number,
  offset: Schema.Number,
  /** The roots a path may be added from. */
  roots: Schema.Array(Schema.String)
})

/** What retiring one capture takes: the rows, the runs and files on disk, and who holds it. */
export const RetirePlanView = Schema.Struct({
  capture: Schema.String,
  /** Rows per table. */
  rows: Schema.Record(Schema.String, Schema.Number),
  runs: Schema.Number,
  bundles: Schema.Number,
  sessionDir: Schema.NullOr(Schema.String),
  artifactDir: Schema.NullOr(Schema.String),
  uploads: Schema.Array(Schema.String),
  /** Running jobs naming the capture: a retire is refused while any does. */
  holders: Schema.Array(Schema.Struct({ jobId: Schema.Number, kind: Schema.String, how: Schema.String })),
  /** Whether the store or the disk holds anything of it. */
  known: Schema.Boolean
})

export class SourcesGroup extends HttpApiGroup.make("sources")
  .add(HttpApiEndpoint.get("sourceList", "/sources", {
    query: {
      /** `1` = only captures that need ingest or need cut. */
      stale: Schema.optional(Schema.String),
      q: Schema.optional(Schema.String),
      limit: Schema.optional(Schema.FiniteFromString),
      offset: Schema.optional(Schema.FiniteFromString)
    },
    success: SourcesPage
  })
    .annotate(OpenApi.Description, "The captures of the population: ingest, cut and standing per lane, paged."))
  .add(HttpApiEndpoint.get("retirePlan", "/sources/:capture/retire-plan", {
    params: { capture: Schema.String },
    success: RetirePlanView
  })
    .annotate(OpenApi.Description, "What retiring one capture would remove, and what holds it."))
  .prefix("/api")
{}

/** One file of the pcap root (`pcaps.ts`): its stat, where the pipeline stands with it, its last run per lane. */
export const PcapView = Schema.Struct({
  file: Schema.String,
  size: Schema.Number,
  modifiedAt: Schema.String,
  /** The capture the population holds it as, or the name its ingest will give it. */
  capture: Schema.String,
  /** new | ingested | cut. */
  state: Schema.String,
  /** The standing cut's cases; null unless cut. */
  cases: Schema.NullOr(Schema.Number),
  ingest: Schema.NullOr(Schema.String),
  cut: Schema.NullOr(Schema.String),
  /** Why the ingest failed or the cut broke. */
  problem: Schema.NullOr(Schema.String),
  standings: Schema.Array(SourceStanding)
})

/** One capture of the inbox (`inbox.ts`): its files and where it stands. */
export const InboxEntryView = Schema.Struct({
  /** The file its anonymised copy takes in the pcap root. */
  copy: Schema.String,
  files: Schema.Array(Schema.Struct({ name: Schema.String, size: Schema.Number, modifiedAt: Schema.String })),
  /** settling | waiting | taking | refused. */
  state: Schema.String,
  /** The job taking it, or the one whose take failed. */
  jobId: Schema.NullOr(Schema.Number)
})

/** How a capture replay fits one lane (`pcap-lanes.ts`). */
export const PcapLaneView = Schema.Struct({
  lane: Schema.String,
  /** fits | forced | none. */
  fit: Schema.String,
  lacks: Schema.Array(Schema.String),
  line: Schema.NullOr(Schema.String)
})

/** The Pcaps page: the pcap root's files, the inbox, the lanes; the roots null outside a workspace. */
export const PcapsView = Schema.Struct({
  pcaps: Schema.NullOr(Schema.String),
  inbox: Schema.NullOr(Schema.String),
  files: Schema.Array(PcapView),
  inboxEntries: Schema.Array(InboxEntryView),
  lanes: Schema.Array(PcapLaneView)
})

export class PcapsGroup extends HttpApiGroup.make("pcaps")
  .add(HttpApiEndpoint.get("pcapList", "/pcaps", { success: PcapsView })
    .annotate(OpenApi.Description, "The files of the workspace's pcap root and inbox, and how a capture replay fits each lane."))
  .prefix("/api")
{}

/** A selector the store cannot answer, or that names no run: the reason, as the compare command states it. */
export class ReportRefused extends Schema.TaggedError<ReportRefused>()("Site.ReportRefused", {
  reason: Schema.String
}) {}

/** The run store did not answer: its message, so a page says why rather than a bare 500. */
export class ReportUnavailable extends Schema.TaggedError<ReportUnavailable>()("Site.ReportUnavailable", {
  reason: Schema.String
}) {}

/**
 * The reports over run sets, compared by stable call identity: the
 * `sip.compare/1` document of two selectors — the one the compare command
 * prints as JSON — and the
 * `sip.run-report/1` document of one. Both cross the wire as `Unknown`: the
 * formats are `@sip/contracts`', published with their JSON Schemas.
 */
export class ReportsGroup extends HttpApiGroup.make("reports")
  .add(HttpApiEndpoint.get("compareReport", "/compare", {
    query: {
      /** The before side's selector (`@develop`, `@env:X`, `@run:N`, …); a query naming none is a 400. */
      a: Schema.optional(Schema.String),
      /** The after side's selector; a query naming none is a 400. */
      b: Schema.optional(Schema.String),
      /** The lane a side that pins none is read on; absent = the configured lane. */
      lane: Schema.optional(Schema.String),
      /** `1` lists every compared call, the unchanged ones too. */
      all: Schema.optional(Schema.String),
      /** `1` lists only the calls whose two ladders differ, whatever their class. */
      ladderChanged: Schema.optional(Schema.String),
      /** How many listed calls get their ladders read. */
      ladders: Schema.optional(Schema.FiniteFromString)
    },
    success: Schema.Unknown,
    error: [ReportRefused.pipe(HttpApiSchema.status(400)), ReportUnavailable.pipe(HttpApiSchema.status(503))]
  })
    .annotate(OpenApi.Description, "Two run sets compared call by call (`sip.compare/1`)."))
  .add(HttpApiEndpoint.get("runReport", "/report", {
    query: {
      /** The run set's selector; `@run:N` of a run the wipe dropped reads its kept summary. */
      run: Schema.optional(Schema.String),
      lane: Schema.optional(Schema.String)
    },
    success: Schema.Unknown,
    error: [ReportRefused.pipe(HttpApiSchema.status(400)), ReportUnavailable.pipe(HttpApiSchema.status(503))]
  })
    .annotate(OpenApi.Description, "One run set's report (`sip.run-report/1`)."))
  .prefix("/api")
{}

/** One failed check of a campaign case: where, what, expected beside seen, what the decision said, whose side. */
export const CampaignCheck = Schema.Struct({
  anchor: Schema.String,
  field: Schema.String,
  expected: Schema.NullOr(Schema.String),
  seen: Schema.NullOr(Schema.String),
  said: Schema.NullOr(Schema.String),
  side: Schema.NullOr(Schema.String),
  /** `value`: expected and seen are two values of one thing; `statement`: seen broke the rule expected states. */
  compare: Schema.optional(Schema.NullOr(Schema.Literals(["value", "statement"]))),
  /** The flow step it was made at; null or absent where none is named. */
  step: Schema.optional(Schema.NullOr(Schema.String))
})

/** One check a case makes, passing or not: the flow step (null for a gate), where, which field (`*`: any the step's others do not name), how, against what. */
export const CampaignChecked = Schema.Struct({
  step: Schema.NullOr(Schema.String),
  anchor: Schema.String,
  field: Schema.String,
  op: Schema.String,
  expected: Schema.NullOr(Schema.String)
})

/** One attachment a holder lists: its id, its name, its kind, what heads it, who attached it. */
export const AttachmentListed = Schema.Struct({
  id: Schema.Number,
  name: Schema.String,
  kind: Attachment.Kind,
  title: Schema.String,
  source: Schema.String
})

/** One attachment read for its kind's component (`@sip/contracts` `Attachment`, ADR-0060 appendix §9). */
export const AttachmentView = Schema.Union([
  /** A log's text, its two ends where it is long, and its whole size. */
  Schema.Struct({ kind: Schema.Literal("log"), text: Schema.String, bytes: Schema.Number }),
  Schema.Struct({ kind: Schema.Literal("records"), records: Schema.Array(Schema.Record(Schema.String, Schema.String)) }),
  /** A `@sip/contracts` `Attachment.Diff`, decoded by it before it is answered. */
  Schema.Struct({ kind: Schema.Literal("diff"), diff: Schema.Unknown }),
  /** A `@sip/contracts` `Attachment.Table`: each row's cells' classes where any cell states one. */
  Schema.Struct({
    kind: Schema.Literal("table"),
    columns: Schema.Array(Schema.String),
    rows: Schema.Array(Schema.Array(Schema.String)),
    classes: Schema.optionalKey(Schema.Array(Schema.Array(Schema.NullOr(Schema.Literals(["ok", "nok", "muted"])))))
  }),
  Schema.Struct({ kind: Schema.Literal("json"), value: Schema.Unknown }),
  Schema.Struct({ kind: Schema.Literal("file"), bytes: Schema.Number }),
  Schema.Struct({ kind: Schema.Literal("link"), url: Schema.String })
])

/** One replay of a cell replayed more than once on its lane: its bundle directory, its outcome, its failed checks, whether it is the result. */
export const CampaignAttempt = Schema.Struct({
  cell: Schema.NullOr(Schema.String),
  outcome: Schema.Literals(["pass", "failure", "error", "skipped"]),
  failed: Schema.Number,
  kept: Schema.Boolean
})

export const CampaignResult = Schema.Struct({
  caseId: Schema.String,
  outcome: Schema.Literals(["pass", "failure", "error"]),
  /** The cell's bundle directory under its lane's session; null where none was written. */
  cell: Schema.NullOr(Schema.String),
  log: Schema.NullOr(Schema.Struct({
    records: Schema.optionalKey(Schema.Number),
    missing: Schema.optionalKey(Schema.String)
  })),
  detail: Schema.NullOr(Schema.String),
  /** The cell's status: its lines' final status, or `error` where no call was judged. */
  status: Verification.CellStatus,
  /** Its verification lines in their order; empty where no call was judged. */
  lines: Schema.Array(VerificationLineView),
  /** What its lane could not check though the cell ran (a claim not requested, a verification not checkable there); absent where it checked everything. */
  unchecked: Schema.optionalKey(Schema.String),
  checks: Schema.Array(CampaignCheck),
  /** Every attempt, in order, of a cell replayed more than once on its lane; absent for one attempt. */
  attempts: Schema.optionalKey(Schema.Array(CampaignAttempt))
})

export const CampaignLane = Schema.Struct({
  lane: Schema.String,
  runId: Schema.Number,
  /** The run's bundle directory under the capture's session tree (`run<id>`). */
  session: Schema.String,
  status: Schema.String,
  detail: Schema.NullOr(Schema.String),
  /** Cases the lane did not run (not ported, excluded). */
  skipped: Schema.Number,
  /** The cases a campaign tag left out on the lane, each with why. */
  excluded: Schema.Array(Schema.Struct({ caseId: Schema.String, reason: Schema.String })),
  /** Per status a cell that did not run holds (`excluded`, `not runnable`, `not ported`), how many. */
  notRun: Schema.Record(Schema.String, Schema.Number),
  results: Schema.Array(CampaignResult)
})

export const CampaignRun = Schema.Struct({
  id: Schema.Number,
  /** The environment that made the run. */
  env: Schema.NullOr(Schema.String),
  startedAt: Schema.String,
  finishedAt: Schema.NullOr(Schema.String),
  /** What the run states it ran on (environment, seed, versions), in the deployment's order. */
  labels: Schema.Array(Schema.Struct({ name: Schema.String, value: Schema.String })),
  lanes: Schema.Array(CampaignLane),
  /** What the run covered where it was not the whole campaign (one case, a sub-case, a glob); null for a full run. */
  scope: Schema.NullOr(Schema.String),
  /** The campaign tags the run opted into and those each lane left out; null where the run states none. */
  selection: Schema.NullOr(Schema.Struct({
    allow: Schema.Array(Schema.String),
    lanes: Schema.Record(Schema.String, Schema.Array(Schema.String))
  })),
  /** Every check each case the run compiled makes, by case id. */
  checked: Schema.Record(Schema.String, Schema.Array(CampaignChecked))
})

export const CampaignCase = Schema.Struct({
  caseId: Schema.String,
  ref: Schema.String,
  subcase: Schema.NullOr(Schema.String),
  family: Schema.String,
  /** What the case tests, in a line; null where no run states it. */
  title: Schema.NullOr(Schema.String),
  description: Schema.NullOr(Schema.String),
  /** The groups the case counts under, 0..N: its service options, its own groups, its tags. */
  groups: Schema.Array(Schema.String),
  /** The campaign tags the case carries, 0..N. */
  tags: Schema.Array(Schema.String),
  /** What the case uses, by role, each with the page it is read on where there is one. */
  links: Schema.Array(Schema.Struct({ role: Schema.String, label: Schema.String, url: Schema.NullOr(Schema.String) })),
  /** The case file the newest run that compiled the case compiled it from; null where no run kept it. */
  source: Schema.NullOr(Schema.Struct({ file: Schema.String, text: Schema.String }))
})

/** One lane a campaign run may name: its id, the group it runs in, why it does not run here (null where it does). */
export const CampaignLaunchLane = Schema.Struct({
  lane: Schema.String,
  group: Schema.String,
  refused: Schema.NullOr(Schema.String)
})

/** One campaign tag the deployment declares: what the page offers it as, the lanes excluding it, whether the campaign opts into it. */
export const CampaignLaunchTag = Schema.Struct({
  tag: Schema.String,
  label: Schema.String,
  warning: Schema.NullOr(Schema.String),
  excludedOn: Schema.Array(Schema.String),
  allowed: Schema.Boolean
})

/** One campaign this server knows, with the title its definition states; null where it states none. */
export const CampaignListed = Schema.Struct({
  capture: Schema.String,
  title: Schema.NullOr(Schema.String)
})

/** One environment's latest runs of a campaign, newest first, with every case any of them ran or a tag left out. */
export const CampaignView = Schema.Struct({
  capture: Schema.String,
  /** The title its definition states; null where it states none. */
  title: Schema.NullOr(Schema.String),
  /** The environment whose runs are read. */
  env: Schema.NullOr(Schema.String),
  /** Every environment that wrote the campaign's runs, the latest writer first. */
  envs: Schema.Array(Schema.String),
  /** The label of a check's `said`. */
  saidLabel: Schema.String,
  /** The name of the cell attachment a check's `said` was read from. */
  saidLog: Schema.String,
  /** The side vocabulary, in the order the pages list it. */
  sides: Schema.Array(Schema.String),
  lanes: Schema.Array(Schema.String),
  runs: Schema.Array(CampaignRun),
  cases: Schema.Array(CampaignCase),
  /**
   * Every lane a run started from the page names, in the catalog's order: the
   * lanes of one group run together, a refused one does not run here; the
   * lanes the campaign states, picked when a start opens, null where it states
   * none. Null where the page reads another environment than the server's.
   */
  launch: Schema.NullOr(Schema.Struct({
    lanes: Schema.Array(CampaignLaunchLane),
    tags: Schema.Array(CampaignLaunchTag),
    defaultLanes: Schema.NullOr(Schema.Array(Schema.String)),
    /** The collection levels a start may name, and the one a start naming none takes. */
    collection: Schema.Struct({ default: Schema.String, levels: Schema.Array(Schema.String) })
  })),
  /** The environment the server starts this campaign's runs in; null where it starts none. */
  own: Schema.NullOr(Schema.String)
})

/** An on-demand collector to run on one cell, addressed as the study lists it, and the case result it lands under. */
export const CollectRequest = Schema.Struct({
  collector: Schema.String,
  capture: Schema.String,
  session: Schema.String,
  cell: Schema.String,
  runId: Schema.Number,
  caseId: Schema.String,
  lane: Schema.String
})

/** What an on-demand run answered of the cell: its lines, how many attachments it landed, who left the lab changed. */
export const CollectAnswered = Schema.Struct({
  verifications: Schema.Array(Verification.Verification),
  attached: Schema.Number,
  leftChanged: Schema.Array(Schema.String)
})

/** Why an on-demand run did not start: a cell the page cannot address, or a collector the lane does not offer. */
export class CollectRefused extends Schema.TaggedError<CollectRefused>()("Site.CollectRefused", {
  reason: Schema.String
}) {}

/** The attachments of a cell, a batch or a job: a holder's list, one read for its kind (its file is `attachment-file.ts`'s). */
export class AttachmentsGroup extends HttpApiGroup.make("attachments")
  .add(HttpApiEndpoint.get("attachmentList", "/attachments", {
    /** The holder: a cell (`run` and `case`), a `batch` or a `job`. */
    query: {
      run: Schema.optional(Schema.FiniteFromString),
      case: Schema.optional(Schema.String),
      batch: Schema.optional(Schema.FiniteFromString),
      job: Schema.optional(Schema.FiniteFromString)
    },
    success: Schema.Array(AttachmentListed),
    error: NotFound.pipe(HttpApiSchema.status(404))
  })
    .annotate(OpenApi.Description, "What a cell (`run` and `case`), a batch (`batch`) or a job (`job`) holds attached, in its order."))
  .add(HttpApiEndpoint.get("attachmentView", "/attachments/:id", {
    params: { id: Schema.FiniteFromString },
    success: AttachmentView,
    error: NotFound.pipe(HttpApiSchema.status(404))
  })
    .annotate(OpenApi.Description, "One attachment read for its kind: a log's two ends, records, a table, a diff, a JSON document, a file's size, a link's URL."))
  .add(HttpApiEndpoint.get("collectOffered", "/collect", {
    query: { lane: Schema.String },
    success: Schema.Array(Schema.String)
  })
    .annotate(OpenApi.Description, "The on-demand collectors a cell of the lane may run, by name."))
  .add(HttpApiEndpoint.post("collectRun", "/collect", {
    payload: CollectRequest,
    success: CollectAnswered,
    error: CollectRefused.pipe(HttpApiSchema.status(400))
  })
    .annotate(OpenApi.Description, "Runs an on-demand collector on one cell: its attachments land on the cell, its lines are answered here and never merged."))
  .prefix("/api")
{}

/** The end-to-end campaigns: their names, one's runs. */
export class CampaignGroup extends HttpApiGroup.make("campaign")
  .add(HttpApiEndpoint.get("campaignList", "/campaign", { success: Schema.Array(CampaignListed) })
    .annotate(OpenApi.Description, "The end-to-end campaigns this server knows: the ones it runs, whether or not they ever ran, and every one the store holds runs of, each with its title."))
  .add(HttpApiEndpoint.get("campaignView", "/campaign/:capture", {
    params: { capture: Schema.String },
    query: {
      limit: Schema.optional(Schema.FiniteFromString),
      env: Schema.optional(Schema.String),
      /** A run to read whatever its age: the one a report's link names. */
      run: Schema.optional(Schema.FiniteFromString)
    },
    success: CampaignView
  })
    .annotate(OpenApi.Description, "One campaign's latest runs in an environment: each lane's results and failed checks."))
  .add(HttpApiEndpoint.get("campaignCaseSource", "/campaign-cases/source", {
    /** The campaign case id, `<ref> [<sub-case>]`. */
    query: { case: Schema.String },
    success: Schema.String.pipe(HttpApiSchema.asText()),
    error: NotFound.pipe(HttpApiSchema.status(404))
  })
    .annotate(OpenApi.Description, "A campaign case's TypeScript file: its intention comment and its input (`case`: `<ref> [<sub-case>]`)."))
  .add(HttpApiEndpoint.get("campaignCasePivot", "/campaign-cases/pivot", {
    query: { case: Schema.String },
    success: Schema.Unknown,
    error: NotFound.pipe(HttpApiSchema.status(404))
  })
    .annotate(OpenApi.Description, "A campaign case's pivot document, as this checkout compiles its sub-case (`case`)."))
  .prefix("/api")
{}

/** A page URL no overview is made of: why, and where the page forms are stated. */
export class LinksRefused extends Schema.TaggedError<LinksRefused>()("Site.LinksRefused", {
  reason: Schema.String,
  llms: Schema.String
}) {}

/** A page URL whose route names a thing this deployment does not hold (a campaign of no run): why, and the site map. */
export class PageMissing extends Schema.TaggedError<PageMissing>()("Site.PageMissing", {
  reason: Schema.String,
  llms: Schema.String
}) {}

/** The overview of a page URL, for a reader that runs no JavaScript. */
export class LinksGroup extends HttpApiGroup.make("links")
  .add(HttpApiEndpoint.get("links", "/links", {
    /** The page URL as pasted: absolute or site-relative. */
    query: { url: Schema.String },
    success: Schema.String.pipe(HttpApiSchema.asText({ contentType: "text/markdown" })),
    error: [LinksRefused.pipe(HttpApiSchema.status(400)), PageMissing.pipe(HttpApiSchema.status(404))]
  })
    .annotate(OpenApi.Description, "The overview of a page URL (`url`): Markdown, JSON with `Accept: application/json`; absolute links to the page's data."))
  .prefix("/api")
{}

/** Every route states a schema refusal's reason (`schema-errors.ts`); groups added after the middleware would not. */
export class Api extends HttpApi.make("site")
  .add(CatalogGroup)
  .add(TriageGroup)
  .add(DeltasGroup)
  .add(CapturesGroup)
  .add(RulesGroup)
  .add(RequestsGroup)
  .add(JobsGroup)
  .add(SourcesGroup)
  .add(PcapsGroup)
  .add(ReportsGroup)
  .add(CampaignGroup)
  .add(AttachmentsGroup)
  .add(LinksGroup)
  .middleware(SchemaErrors)
{}
