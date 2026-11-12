/**
 * A lane's view: what its `lane.v1` file states, as {@link Lanes.load} read
 * it, typed for the code that drives the lane, before a pack reads the parts
 * it names. One lane is simulated and built in: its system under test (SUT)
 * runs in process, so its file states no SIP target, backend, pacing, timers,
 * known bugs or order, and no other lane runs on the simulated clock. Every
 * other lane is hosted: a SUT the rig reaches over SIP on the wall clock.
 */
import { Check } from "@sip/contracts"
import * as Result from "effect/Result"
import * as Schema from "effect/Schema"
import * as Lane from "./lane.js"
import * as Lanes from "./lanes.js"
import { decoderOf, type Refusal, refuse } from "./manifest-file.js"

/** The SUT's timers a case names, in ms, by their lane-file name (`keepalive_ms`). */
export type Timers = Readonly<Record<string, number>>

/** The campaign tags a lane treats otherwise than each tag's own default. */
export interface TagLists {
  readonly run: ReadonlyArray<string>
  readonly exclude: ReadonlyArray<string>
}

/** A lane stating no tag list. */
export const NO_TAG_LISTS: TagLists = { run: [], exclude: [] }

/** The SUT's backend budget per request kind, and the one-way transit (`[backend.http_budget]`). */
export interface HttpBudget {
  readonly new_call_ms: number
  readonly follow_up_ms: number
  readonly transit_ms: number
}

/** A hosted lane's SUT as its file's `[sip]`, `[backend]`, `[pacing]` and `known_bugs` state it; `name` and `file` are the lane's. */
export interface Target {
  readonly name: string
  readonly file: string
  /** The SUT in one line, for a reader of a result: the lane's `description`, else its id. */
  readonly description: string
  /** `managed`: the rig starts the SUT per batch; `standing`: up before the run, up after it. */
  readonly lifecycle: Lane.Lifecycle
  readonly ingress: string
  readonly endpoint_host: string
  readonly exclusive?: boolean
  readonly known_bugs?: ReadonlyArray<Check.KnownBug>
  readonly http_budget: HttpBudget
  readonly knobs?: { readonly concurrency?: number; readonly batch_size?: number; readonly starts_per_second?: number }
  readonly ceilings?: { readonly cells?: number; readonly starts_per_second?: number }
  readonly campaign?: { readonly concurrency: number; readonly starts_per_second: number; readonly timing_tolerance_ms: number }
}

/** What every lane's view holds. */
interface Common {
  readonly id: string
  /** It as {@link Lanes.load} loaded it. */
  readonly loaded: Lanes.Loaded
  readonly capabilities: ReadonlySet<string>
  /** Its SUT's timers. */
  readonly sutTimers: Timers
  readonly tags: TagLists
}

/** The built-in simulated lane. */
export interface Simulated extends Common {
  readonly kind: "fake"
  /** Its own file, where the deployment states one. */
  readonly file: string | undefined
}

/** A hosted lane, read from its file. */
export interface Hosted extends Common {
  readonly kind: "real"
  readonly file: string
  readonly sip: Target
  readonly backendMode: Lane.BackendMode
  /** Where the rig serves or relays the backend; absent where the mode is `none`. */
  readonly backendListen: string | undefined
}

/** Any lane's view. */
export type View = Simulated | Hosted

/** The built-in lane `id` where no file states it: simulated, naming no part. */
export const builtIn = (id: string): Lanes.Loaded => ({
  id,
  file: "built in, no file",
  lane: { schema: Lane.SCHEMA, clock: "simulated" },
  parts: new Map(),
  operations: new Map(),
  capabilities: Lanes.capabilitiesOf({ schema: Lane.SCHEMA, clock: "simulated" }, [], [])
})

/** The built-in lane `id` with no file, naming no part, its in-process SUT's timers `timers`. */
export const simulatedBuiltIn = (id: string, timers: Timers): Simulated => {
  const loaded = builtIn(id)
  return { kind: "fake", id, file: undefined, loaded, capabilities: loaded.capabilities, sutTimers: timers, tags: NO_TAG_LISTS }
}

const tagListsOf = (lane: Lane.LaneFile): TagLists => ({ run: lane.tags?.run ?? [], exclude: lane.tags?.exclude ?? [] })

/** The keys of a lane file the built-in lane's own file may not state: its SUT is the in-process worker. */
const SIMULATED_FIXED = ["timers", "pacing", "known_bugs", "order"] as const

/**
 * The built-in lane as its own file `loaded` states it, its in-process SUT's
 * timers `timers`: refused where its clock is not simulated or it states a
 * key its in-process SUT fixes.
 */
export const simulatedOf = (loaded: Lanes.Loaded, timers: Timers): Result.Result<Simulated, Refusal> => {
  if (loaded.lane.clock !== "simulated") return Result.fail(refuse([loaded.file], `${loaded.id} is the built-in lane: its clock is simulated`))
  const fixed = SIMULATED_FIXED.find((key) => loaded.lane[key] !== undefined)
  if (fixed !== undefined) {
    return Result.fail(refuse([loaded.file], `${fixed}: ${loaded.id} is the built-in lane, its SUT the in-process worker`))
  }
  return Result.succeed({
    kind: "fake",
    id: loaded.id,
    file: loaded.file,
    loaded,
    capabilities: loaded.capabilities,
    sutTimers: timers,
    tags: tagListsOf(loaded.lane)
  })
}

const decodeKnownBugs = decoderOf(Schema.Array(Check.KnownBug))

/** The hosted lane `loaded` states, refused where it runs on the simulated clock, which is `simulatedId`'s alone. */
export const hostedOf = (loaded: Lanes.Loaded, simulatedId: string): Result.Result<Hosted, Refusal> => {
  const { lane, file } = loaded
  if (lane.clock === "simulated") return Result.fail(refuse([file], `clock: the one simulated lane is the built-in ${simulatedId}`))
  const sip = lane.sip!
  const backend = lane.backend!
  const knownBugs = lane.known_bugs === undefined ? undefined : decodeKnownBugs(lane.known_bugs)
  if (knownBugs !== undefined && Result.isFailure(knownBugs)) return Result.fail(refuse([file], `known_bugs.${knownBugs.failure}`))
  const { ceilings, campaign, ...knobs } = lane.pacing ?? {}
  return Result.succeed({
    kind: "real",
    id: loaded.id,
    file,
    sip: {
      name: loaded.id,
      file,
      description: lane.description ?? loaded.id,
      lifecycle: sip.lifecycle,
      ingress: sip.ingress,
      endpoint_host: sip.endpoint_host,
      exclusive: sip.exclusive,
      known_bugs: knownBugs?.success,
      http_budget: backend.http_budget,
      knobs: lane.pacing === undefined ? undefined : knobs,
      ceilings,
      campaign
    },
    loaded,
    backendMode: backend.mode,
    backendListen: backend.listen,
    capabilities: loaded.capabilities,
    sutTimers: lane.timers ?? {},
    tags: tagListsOf(lane)
  })
}
