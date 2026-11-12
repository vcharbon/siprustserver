/**
 * A cell's `cell.v1` description built from its run bundle and what the base
 * knows of where it ran. The bundle gives the clock (`run-config.json`), the
 * lane-clock instants (`timing.json`), the legs and every Call-ID the test
 * side sent or received (`recording/<leg>.jsonl`), and the leg roles
 * (`pivot.json`). The user keys are the identities each recorded line states
 * (`sip-message`'s reading); a line stating none adds none. The wall clock is
 * `timing.json`'s window, else the window a `sut-join.json` states; a bundle
 * stating neither has no description. The claims are those the document
 * states, then the placement's; the keys and records a narrower scope added
 * are read from the cell's directory (`cell-records.ts`), then the
 * placement's. A `keys.json` or a `joined/` present that cannot be read
 * leaves the bundle with no description: the cell is then input a scope was
 * not handed whole.
 */
import { Batch, Bundle, Postcondition } from "@sip/contracts"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Option from "effect/Option"
import * as Result from "effect/Result"
import * as Schema from "effect/Schema"
import * as path from "node:path"
import { BASE_KEYS, CALL_ID, type Cell, type Instant, type Leg, SCHEMA, type Verification } from "./cell.js"
import * as CellRecords from "./cell-records.js"

/** Why a bundle yields no cell description. */
export class Unreadable extends Schema.TaggedError<Unreadable>()("Rig.CellUnreadable", {
  dir: Schema.String,
  reason: Schema.String
}) {
  override get message(): string {
    return `${this.dir}: ${this.reason}`
  }
}

/** What the base knows of a cell that its bundle does not state. */
export interface Placement {
  readonly id: string
  readonly case: string
  readonly subCase?: string | undefined
  readonly lane: string
  readonly batch: string
  readonly attempt: number
  readonly claims?: Readonly<Record<string, unknown>> | undefined
  /** The keys the case declares and the registration seam or earlier collectors added. */
  readonly keys?: Readonly<Record<string, ReadonlyArray<string>>> | undefined
  readonly records?: Readonly<Record<string, string>> | undefined
  readonly verifications?: ReadonlyArray<Verification> | undefined
  readonly tags?: ReadonlyArray<string> | undefined
  readonly needs?: ReadonlyArray<string> | undefined
}

/** The parts of `pivot.json` that name each leg's actor and each actor's kind. */
const Roster = Schema.Struct({
  legs: Schema.Array(Schema.Struct({ id: Schema.String, actor: Schema.String })),
  actors: Schema.Array(Schema.Struct({ id: Schema.String, kind: Schema.String }))
})
const decodeRoster = Schema.decodeUnknownOption(Roster)

const decodePostconditions = Schema.decodeUnknownOption(Schema.Struct({ postconditions: Schema.optionalKey(Postcondition.Postconditions) }))

/**
 * The claims the document `raw` states (`postconditions`), empty where it
 * states none; none where its postconditions do not decode.
 */
const statedClaimsOf = (raw: unknown): Option.Option<Readonly<Record<string, unknown>>> =>
  Option.map(decodePostconditions(raw), (document) => Postcondition.statedClaims(document.postconditions))

/** One recorded datagram of a leg, its head facts read. */
interface Recorded {
  readonly atUs: number
  readonly seq: number
  readonly head: Bundle.RecordedHead
  readonly identities: Bundle.RecordedIdentities | undefined
}

const distinct = (values: Iterable<string | undefined>): Array<string> => [
  ...new Set([...values].filter((value): value is string => value !== undefined && value !== ""))
]

/** The first INVITE a leg sent or received. */
const inviteOf = (messages: ReadonlyArray<Recorded>): Recorded | undefined =>
  messages.find((message) => message.head.method === "INVITE")

/** One leg's description from its datagrams, in arrival order. */
const legOf = (id: string, messages: ReadonlyArray<Recorded>, actor: string, role: string): Leg => {
  const identities = inviteOf(messages)?.identities
  const answer = messages.find(({ head }) =>
    head.status !== undefined && head.status >= 200 && head.status < 300 && head.cseqMethod === "INVITE"
  )
  const callId = messages.find((message) => message.head.callId !== undefined)?.head.callId
  return {
    id,
    role,
    actor,
    ...(callId === undefined ? {} : { call_id: callId }),
    ...(identities === undefined ? {} : { from: identities.from.uri, to: identities.to.uri }),
    ...(identities?.ruri === undefined ? {} : { request_uri: identities.ruri.uri }),
    ...(messages.length === 0 ? {} : { start_us: messages[0]!.atUs, end_us: messages[messages.length - 1]!.atUs }),
    ...(answer === undefined ? {} : { answer_us: answer.atUs })
  }
}

/** The keys every cell carries: every Call-ID, and the users each leg's first INVITE names. */
const baseKeysOf = (messages: ReadonlyArray<Recorded>, legs: ReadonlyArray<ReadonlyArray<Recorded>>): Record<string, Array<string>> => {
  const invites = legs.map((leg) => inviteOf(leg)?.identities)
  return {
    [CALL_ID]: distinct(messages.map((message) => message.head.callId)),
    caller: distinct(invites.map((identities) => identities?.from.user ?? undefined)),
    callee: distinct(invites.map((identities) => identities?.to.user ?? undefined)),
    request_uri_user: distinct(invites.map((identities) => identities?.ruri?.user ?? undefined))
  }
}

/** The base keys with the declared ones merged in, each list without repeats. */
const keysOf = (
  base: Record<string, Array<string>>,
  declared: Readonly<Record<string, ReadonlyArray<string>>>
): Record<string, Array<string>> => {
  const out: Record<string, Array<string>> = { ...base }
  for (const [name, values] of Object.entries(declared)) out[name] = distinct([...(out[name] ?? []), ...values])
  for (const name of BASE_KEYS) out[name] ??= []
  return out
}

/**
 * The wall-clock window of a run, taken whole from one source: `timing.json`'s
 * when it states both instants, else the SUT join's; none when neither does.
 */
const wallWindowOf = (
  timing: Bundle.RunTiming,
  join: Batch.SutJoin | undefined
): { readonly start: number; readonly end: number } | undefined => {
  if (timing.wall_started_at_ms !== undefined && timing.wall_ended_at_ms !== undefined) {
    return { start: timing.wall_started_at_ms, end: timing.wall_ended_at_ms }
  }
  return join === undefined ? undefined : { start: join.started_at_ms, end: join.ended_at_ms }
}

/** The description of the cell whose bundle is `dir`, placed as `placement` says. */
export const ofBundle = (dir: string, placement: Placement): Effect.Effect<Cell, Unreadable, FileSystem.FileSystem> =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const unreadable = (reason: string) => new Unreadable({ dir, reason })
    const json = (file: string) =>
      fs.readFileString(path.join(dir, file)).pipe(
        Effect.flatMap((text) => Effect.try(() => JSON.parse(text) as unknown)),
        Effect.mapError(() => unreadable(`${file}: not readable JSON`))
      )
    const config = yield* json("run-config.json").pipe(
      Effect.flatMap((raw) => Bundle.decodeRunConfig(raw).pipe(Effect.mapError((error) => unreadable(`run-config.json: ${error.message}`))))
    )
    const timing = yield* json("timing.json").pipe(
      Effect.flatMap((raw) => Bundle.decodeRunTiming(raw).pipe(Effect.mapError((error) => unreadable(`timing.json: ${error.message}`))))
    )
    const join = yield* json(Batch.SUT_JOIN_FILE).pipe(
      Effect.map((raw) => Option.getOrUndefined(Batch.decodeSutJoinOption(raw))),
      Effect.orElseSucceed(() => undefined)
    )
    const roster = yield* json("pivot.json").pipe(
      Effect.map((raw) => Option.getOrUndefined(decodeRoster(raw))),
      Effect.orElseSucceed(() => undefined)
    )
    const present = (file: string) => fs.exists(path.join(dir, file)).pipe(Effect.mapError((error) => unreadable(`${file}: ${error.message}`)))
    const stated = (yield* present("pivot.json"))
      ? yield* json("pivot.json").pipe(
        Effect.flatMap((raw) =>
          Option.match(statedClaimsOf(raw), {
            onNone: () => Effect.fail(unreadable("pivot.json: postconditions not readable")),
            onSome: Effect.succeed
          })
        )
      )
      : {}
    const keysText = (yield* present(CellRecords.KEYS_FILE))
      ? yield* fs.readFileString(path.join(dir, CellRecords.KEYS_FILE)).pipe(
        Effect.mapError((error) => unreadable(`${CellRecords.KEYS_FILE}: ${error.message}`))
      )
      : undefined
    const added = CellRecords.addedKeysOf(keysText)
    if (Result.isFailure(added)) return yield* Effect.fail(unreadable(`${CellRecords.KEYS_FILE}: ${added.failure}`))
    const joinedFiles = (yield* present(CellRecords.JOINED_DIR))
      ? yield* fs.readDirectory(path.join(dir, CellRecords.JOINED_DIR)).pipe(
        Effect.mapError((error) => unreadable(`${CellRecords.JOINED_DIR}: ${error.message}`))
      )
      : []
    const joined = joinedFiles
      .filter((file) => file.endsWith(".jsonl"))
      .sort()
      .map((file) => [file.slice(0, -".jsonl".length), CellRecords.joinedFileOf(dir, file.slice(0, -".jsonl".length))] as const)
    const recordingDir = path.join(dir, "recording")
    const legIds = (yield* fs.readDirectory(recordingDir).pipe(Effect.orElseSucceed((): ReadonlyArray<string> => [])))
      .filter((file) => file.endsWith(".jsonl"))
      .map((file) => file.slice(0, -".jsonl".length))
      .sort()
    const byLeg: Array<ReadonlyArray<Recorded>> = []
    for (const leg of legIds) {
      const file = `recording/${leg}.jsonl`
      const lines = yield* fs.readFileString(path.join(dir, file)).pipe(
        Effect.flatMap((text) => Effect.suspend(() => Bundle.decodeRecording(text))),
        Effect.mapError((error) => unreadable(`${file}: ${error instanceof Error ? error.message : "not readable"}`))
      )
      byLeg.push(lines.map((line) => ({
        atUs: line.at_us,
        seq: line.seq,
        head: Bundle.recordedHeadOf(line),
        identities: line.identities
      })))
    }
    const messages = byLeg.flat().sort((left, right) => left.atUs - right.atUs || left.seq - right.seq)

    const wall = wallWindowOf(timing, join)
    if (wall === undefined) {
      return yield* Effect.fail(unreadable(`neither timing.json nor ${Batch.SUT_JOIN_FILE} states the wall clock`))
    }
    const lastUs = messages.reduce((last, message) => Math.max(last, message.atUs), 0)
    const start: Instant = { lane_ms: timing.started_at_ms, wall_ms: wall.start }
    const end: Instant = { lane_ms: timing.settled_at_ms ?? Math.max(timing.started_at_ms, lastUs / 1000), wall_ms: wall.end }

    const actorOf = new Map((roster?.legs ?? []).map((leg) => [leg.id, leg.actor]))
    const kindOf = new Map((roster?.actors ?? []).map((actor) => [actor.id, actor.kind]))
    const legs = legIds.map((id, index) => {
      const actor = actorOf.get(id) ?? id
      return legOf(id, byLeg[index]!, actor, kindOf.get(actor) ?? "unknown")
    })
    const cell: Cell = {
      schema: SCHEMA,
      id: placement.id,
      case: placement.case,
      ...(placement.subCase === undefined ? {} : { sub_case: placement.subCase }),
      lane: placement.lane,
      batch: placement.batch,
      attempt: placement.attempt,
      clock: config.clock === "virtual" ? "simulated" : "real",
      keys: keysOf(keysOf(baseKeysOf(messages, byLeg), added.success), placement.keys ?? {}),
      legs,
      timeline: { start, end },
      claims: { ...stated, ...placement.claims },
      records: { ...Object.fromEntries(joined), ...placement.records },
      verifications: [...(placement.verifications ?? [])],
      dir,
      tags: [...(placement.tags ?? [])],
      needs: [...(placement.needs ?? [])]
    }
    return cell
  })
