/**
 * A cell's `cell.v1` description built from its run bundle and what the base
 * knows of where it ran. The bundle gives the clock (`run-config.json`), the
 * lane-clock instants (`timing.json`), the legs and every Call-ID the test
 * side sent or received (`recording/<leg>.jsonl`), and the leg roles
 * (`pivot.json`). The wall clock is `timing.json`'s, else the window a
 * `sut-join.json` states; a bundle stating neither has no description.
 */
import { Batch, Bundle } from "@sip/contracts"
import { parseRecording, type RecordedMessage } from "@sip/viewer-core/recording"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Option from "effect/Option"
import * as Schema from "effect/Schema"
import * as path from "node:path"
import { BASE_KEYS, CALL_ID, type Cell, type Instant, type Leg, SCHEMA, type Verification } from "./cell.js"

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

/** The URI a From or To value names: inside its angle brackets, else before its parameters. */
export const uriOf = (value: string): string => {
  const bracketed = /<([^>]*)>/.exec(value)
  return (bracketed?.[1] ?? value.split(";")[0] ?? value).trim()
}

/** The user part of a SIP URI or the number of a tel URI; none for a URI with no user. */
export const userOf = (uri: string): string | undefined => {
  const sip = /^sips?:([^@]+)@/i.exec(uri)
  if (sip !== null) return sip[1]
  const tel = /^tel:([^;]+)/i.exec(uri)
  return tel === null ? undefined : tel[1]
}

const headerOf = (message: RecordedMessage, ...names: ReadonlyArray<string>): string | undefined =>
  message.headers.find((header) => names.includes(header.name.toLowerCase()))?.value

const distinct = (values: Iterable<string | undefined>): Array<string> => [
  ...new Set([...values].filter((value): value is string => value !== undefined && value !== ""))
]

/** One leg's description from its datagrams, in arrival order. */
const legOf = (id: string, messages: ReadonlyArray<RecordedMessage>, actor: string, role: string): Leg => {
  const invite = messages.find((message) => message.method === "INVITE")
  const answer = messages.find((message) =>
    message.status !== undefined && message.status >= 200 && message.status < 300 && message.cseqMethod === "INVITE"
  )
  const from = invite === undefined ? undefined : headerOf(invite, "from", "f")
  const to = invite === undefined ? undefined : headerOf(invite, "to", "t")
  const callId = messages.find((message) => message.callId !== undefined)?.callId
  return {
    id,
    role,
    actor,
    ...(callId === undefined ? {} : { call_id: callId }),
    ...(from === undefined ? {} : { from: uriOf(from) }),
    ...(to === undefined ? {} : { to: uriOf(to) }),
    ...(invite?.raw === undefined ? {} : { request_uri: invite.raw.split(/\r?\n/)[0]?.split(" ")[1] ?? "" }),
    ...(messages.length === 0 ? {} : { start_us: messages[0]!.atUs, end_us: messages[messages.length - 1]!.atUs }),
    ...(answer === undefined ? {} : { answer_us: answer.atUs })
  }
}

/** The keys every cell carries, read off its legs' datagrams. */
const baseKeysOf = (messages: ReadonlyArray<RecordedMessage>, legs: ReadonlyArray<Leg>): Record<string, Array<string>> => ({
  [CALL_ID]: distinct(messages.map((message) => message.callId)),
  caller: distinct(legs.map((leg) => (leg.from === undefined ? undefined : userOf(leg.from)))),
  callee: distinct(legs.map((leg) => (leg.to === undefined ? undefined : userOf(leg.to)))),
  request_uri_user: distinct(legs.map((leg) => (leg.request_uri === undefined ? undefined : userOf(leg.request_uri))))
})

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
    const recordingDir = path.join(dir, "recording")
    const files = (yield* fs.readDirectory(recordingDir).pipe(Effect.orElseSucceed((): ReadonlyArray<string> => [])))
      .filter((file) => file.endsWith(".jsonl"))
    const texts: Record<string, string> = {}
    for (const file of files) {
      texts[file.slice(0, -".jsonl".length)] = yield* fs.readFileString(path.join(recordingDir, file)).pipe(
        Effect.mapError(() => unreadable(`recording/${file}: not readable`))
      )
    }
    const recording = parseRecording(texts)

    const wallStart = timing.wall_started_at_ms ?? join?.started_at_ms
    const wallEnd = timing.wall_ended_at_ms ?? join?.ended_at_ms
    if (wallStart === undefined || wallEnd === undefined) {
      return yield* Effect.fail(unreadable(`neither timing.json nor ${Batch.SUT_JOIN_FILE} states the wall clock`))
    }
    const lastUs = recording.messages.reduce((last, message) => Math.max(last, message.atUs), 0)
    const start: Instant = { lane_ms: timing.started_at_ms, wall_ms: wallStart }
    const end: Instant = { lane_ms: timing.settled_at_ms ?? Math.max(timing.started_at_ms, lastUs / 1000), wall_ms: wallEnd }

    const actorOf = new Map((roster?.legs ?? []).map((leg) => [leg.id, leg.actor]))
    const kindOf = new Map((roster?.actors ?? []).map((actor) => [actor.id, actor.kind]))
    const legs = recording.legs.map((id) => {
      const actor = actorOf.get(id) ?? id
      return legOf(id, recording.messages.filter((message) => message.leg === id), actor, kindOf.get(actor) ?? "unknown")
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
      keys: keysOf(baseKeysOf(recording.messages, legs), placement.keys ?? {}),
      legs,
      timeline: { start, end },
      claims: { ...placement.claims },
      records: { ...placement.records },
      verifications: [...(placement.verifications ?? [])],
      dir,
      tags: [...(placement.tags ?? [])],
      needs: [...(placement.needs ?? [])]
    }
    return cell
  })
