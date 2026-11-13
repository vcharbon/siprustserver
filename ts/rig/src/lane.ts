/**
 * The lane file, `lanes/<id>.toml` (`lane.v1`, ADR-0060 appendix §11): what
 * the base reads of one place a system under test runs. Its clock; on a real
 * lane its SIP target (`[sip]`) and backend (`[backend]`); its pacing, its
 * system's timers a case names, the failures it waives, its order, its tags;
 * the parts it names by kind (`[parts]`, `part.ts`); its collectors' wiring
 * (`[collectors]`, `collection.ts`) and the operations bound to the names a
 * case asks for (`[operations]`). Every other property is a part.
 *
 * A file stating no `schema` is refused first, as of another format
 * ({@link NO_SCHEMA}); then a key the schema does not name, or a value of the
 * wrong shape, is refused naming the file and the dotted key.
 */
import * as Result from "effect/Result"
import * as Schema from "effect/Schema"
import * as Collection from "./collection.js"
import { decoderOf, Name, Params, type Refusal, refuse, Text, tomlOf } from "./manifest-file.js"

/** The schema version a lane file states. */
export const SCHEMA = "lane.v1"

/** Why a file stating no `schema` is no lane file of this format. */
export const NO_SCHEMA = `schema: the file states none, so it is no ${SCHEMA} file: rewrite it in ${SCHEMA} (ADR-0060 appendix §11)`

/** The directory of the lane files under a workspace or a pack's lanes. */
export const LANES_DIR = "lanes"

/** `real`: the system runs on the wall clock; `simulated`: it runs in process on a paused clock. */
export const CLOCKS = ["real", "simulated"] as const
export type Clock = (typeof CLOCKS)[number]

/** `managed`: the base stands the system up per batch; `standing`: it is up before the run and after it. */
export const LIFECYCLES = ["managed", "standing"] as const
export type Lifecycle = (typeof LIFECYCLES)[number]

/** How the system reaches its backend: the rig serves it, relays it, or is not on its path. */
export const BACKEND_MODES = ["served", "proxied", "none"] as const
export type BackendMode = (typeof BACKEND_MODES)[number]

/** A `host:port` socket address. */
export const HostPort = Schema.String.check(Schema.isPattern(/^(\[[0-9a-fA-F:.]+\]|[^\s:[\]]+):\d{1,5}$/, { title: "host:port" }))

const Count = Schema.Int.check(Schema.isGreaterThanOrEqualTo(1))

const Millis = Schema.Int.check(Schema.isGreaterThanOrEqualTo(0))

const Rate = Schema.Number.check(Schema.isGreaterThanOrEqualTo(0))

/** A timer's name: lower-case letters, digits and underscores, its unit its suffix (`keepalive_ms`). */
const TimerName = Schema.String.check(Schema.isPattern(/^[a-z][a-z0-9_]*$/, { title: "a timer name of lower-case letters, digits and underscores" }))

const Sip = Schema.Struct({
  lifecycle: Schema.Literals(LIFECYCLES),
  /** Where the system takes SIP. */
  ingress: HostPort,
  /** Where the lane's endpoints bind, each on a port the OS places. */
  endpoint_host: Text,
  /** Whether one invocation at a time may drive it; default true. */
  exclusive: Schema.optionalKey(Schema.Boolean),
  /** What brings a standing system up when nothing takes SIP on its ingress, said where that refuses. */
  up: Schema.optionalKey(Text)
})

/** The system's budget for one backend request: one opening a call, one following it, and the one-way transit. */
const HttpBudget = Schema.Struct({
  new_call_ms: Count,
  follow_up_ms: Count,
  transit_ms: Millis
})

const Backend = Schema.Struct({
  mode: Schema.Literals(BACKEND_MODES),
  /** Where the rig serves or relays the backend; stated unless `mode = "none"`. */
  listen: Schema.optionalKey(HostPort),
  http_budget: HttpBudget,
  /** What may hold `listen` on the box, named where the address is found taken. */
  holder: Schema.optionalKey(Text)
})

/** The most a lane takes, whatever a caller asks for. */
const Ceilings = Schema.Struct({
  cells: Schema.optionalKey(Count),
  starts_per_second: Schema.optionalKey(Rate)
})

/** How a campaign paces its cells on the lane, and the timer tolerance its system needs. */
const CampaignPacing = Schema.Struct({
  concurrency: Count,
  starts_per_second: Rate,
  timing_tolerance_ms: Millis
})

/** What a replay takes where nobody states it, the lane's ceilings, and a campaign's pace. */
const Pacing = Schema.Struct({
  concurrency: Schema.optionalKey(Count),
  batch_size: Schema.optionalKey(Count),
  starts_per_second: Schema.optionalKey(Rate),
  ceilings: Schema.optionalKey(Ceilings),
  campaign: Schema.optionalKey(CampaignPacing)
})

/** The tags a lane treats otherwise than each tag's own default. */
const Tags = Schema.Struct({
  /** Tags whose cases this lane runs, whatever the tag's default. */
  run: Schema.optionalKey(Schema.Array(Text)),
  /** Tags whose cases this lane leaves out, whatever the tag's default. */
  exclude: Schema.optionalKey(Schema.Array(Text))
})

/**
 * The operation of the path a name runs, with parameters of the lane's own
 * over its manifest's for a case's step: `[operations.<name>]`.
 */
export const Binding = Schema.Struct({
  operation: Name,
  params: Schema.optionalKey(Params)
})
export interface Binding extends Schema.Schema.Type<typeof Binding> {}

/** A binding as a lane file states it: the operation's name alone, or a {@link Binding}. */
export const bindingOf = (bound: string | Binding): Binding => (typeof bound === "string" ? { operation: bound } : bound)

const LaneShape = Schema.Struct({
  schema: Schema.Literal(SCHEMA),
  /** The lane in one line, for a reader of a result. */
  description: Schema.optionalKey(Text),
  clock: Schema.Literals(CLOCKS),
  /** Where the lane sorts among the others, lowest first; then by id. */
  order: Schema.optionalKey(Schema.Int),
  sip: Schema.optionalKey(Sip),
  backend: Schema.optionalKey(Backend),
  pacing: Schema.optionalKey(Pacing),
  /** The system's own timers a case may name, each in ms. */
  timers: Schema.optionalKey(Schema.Record(TimerName, Millis)),
  /** Failures this lane waives. */
  known_bugs: Schema.optionalKey(Schema.Array(Text)),
  /** Per part kind, the name of the part file (`targets/<kind>/<name>.toml`). */
  parts: Schema.optionalKey(Schema.Record(Name, Name)),
  tags: Schema.optionalKey(Tags),
  collectors: Schema.optionalKey(Schema.Record(Schema.String, Schema.Unknown)),
  /** Per name a case asks for, the operation of the path it runs, its name or its {@link Binding}. */
  operations: Schema.optionalKey(Schema.Record(Name, Schema.Union([Name, Binding])))
})

/** A lane file as read: its `[collectors]` table read by {@link Collection.laneCollectorsOf}. */
export interface LaneFile extends Omit<Schema.Schema.Type<typeof LaneShape>, "collectors"> {
  readonly collectors?: Collection.LaneCollectors
}

const decode = decoderOf(LaneShape)

/** The rules a shape cannot state. */
const meaningOf = (lane: LaneFile): string | undefined => {
  if (lane.clock === "simulated") {
    if (lane.sip !== undefined) return "sip: a simulated lane's system runs in process, on no SIP target"
    if (lane.backend !== undefined) return "backend: a simulated lane's backend is the registered double"
  } else {
    if (lane.sip === undefined) return "sip: a real lane states its SIP target"
    if (lane.backend === undefined) return "backend: a real lane states how its system reaches its backend"
  }
  const backend = lane.backend
  if (backend !== undefined && backend.mode !== "none" && backend.listen === undefined) {
    return `backend.listen: a lane whose backend is ${backend.mode} states where the rig listens for it`
  }
  if (backend?.mode === "none" && backend.listen !== undefined) {
    return "backend.listen: a lane whose backend is none has no rig on its system's backend path"
  }
  const both = (lane.tags?.run ?? []).filter((tag) => (lane.tags?.exclude ?? []).includes(tag))
  if (both.length > 0) return `tags: ${both.join(", ")} stands in both run and exclude`
  return undefined
}

/** `raw` with its `[collectors]` table read, or why the table does not hold (`collectors.<key>: …`). */
const withCollectors = (raw: Schema.Schema.Type<typeof LaneShape>): Result.Result<LaneFile, string> => {
  const { collectors, ...rest } = raw
  if (collectors === undefined) return Result.succeed(rest)
  return Result.mapBoth(Collection.laneCollectorsOf(collectors), {
    onFailure: (reason) => `collectors.${reason}`,
    onSuccess: (read) => ({ ...rest, collectors: read })
  })
}

/** The `schema` the TOML value `raw` states, `undefined` where it states none. */
const schemaOf = (raw: unknown): unknown => (typeof raw === "object" && raw !== null ? (raw as { readonly schema?: unknown }).schema : undefined)

/** Whether the TOML value `raw` states no `schema`: the file is of a format before this one. */
const unschemed = (raw: unknown): boolean => schemaOf(raw) === undefined

/**
 * Whether `text` is TOML stating no `schema` key, the file {@link parse}
 * refuses with {@link NO_SCHEMA}: false for text that is no TOML and for a
 * file stating any schema, `lane.v1` or another.
 */
export const statesNoSchema = (text: string): boolean => {
  const raw = tomlOf(text)
  return Result.isSuccess(raw) && unschemed(raw.success)
}

/** The lane file `file` holds as `text`, or why it is none. */
export const parse = (file: string, text: string): Result.Result<LaneFile, Refusal> => {
  const stated = Result.flatMap(tomlOf(text), (raw) => (unschemed(raw) ? Result.fail(NO_SCHEMA) : Result.succeed(raw)))
  const read = Result.flatMap(Result.flatMap(stated, decode), withCollectors)
  if (Result.isFailure(read)) return Result.fail(refuse([file], read.failure))
  const why = meaningOf(read.success)
  return why === undefined ? Result.succeed(read.success) : Result.fail(refuse([file], why))
}
