/**
 * Who may touch a deployment's shared things at once.
 *
 * What is shared is the BOX, never the session tree: run bundles are named by
 * the store, cases by their content and a prep number comes from a sequence,
 * so no two commands write over each other's files. Two kinds of thing are
 * left, shared differently:
 *
 * - **a box-wide singleton** — a lane whose SUT is one per host, a build that
 *   one host can run once at a time — takes a named exclusive lock under the
 *   deployment's lock root (`Roots.locks`), outside every checkout, so two
 *   checkouts contend for it too;
 * - **the box's QUIET**, the one shared thing that is about CPU: a real-clock
 *   lane measures wall time, so a neighbour's cells would time its cells out.
 *   The box takes a readers-writer lock: a real-clock replay holds it alone,
 *   every other replay holds it with the rest.
 *
 * A readers-writer lock is writer-preferring: the writer's marker goes in
 * first and holds new readers off, then the readers already in are waited out.
 * A caller that will not wait is refused BY NAME, never queued silently.
 *
 * A lock whose holder is gone is stale and is broken with a line saying so:
 * a killed sweep never needs a human to un-wedge the next one.
 *
 * Taking a lock polls on the fiber; the caller's `watch` hears each blocker
 * as it changes, and `undefined` once the lock is taken or refused, so a
 * job's row can say whom it waits for. The blocker is printed every
 * {@link REPORT_MS} besides.
 */
import { Liveness } from "@sip/toolchain"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as path from "node:path"
import { Roots } from "./deployment/index.js"

/** The lock, taken: releasing is idempotent, so a double release is harmless. */
export interface Taken {
  readonly release: Effect.Effect<void>
}

/** The lock, refused: `held` names who has it and since when. */
export interface Held {
  readonly held: string
}

/** How long a caller waits for the one in its way. Zero refuses on the spot. */
export interface Patience {
  readonly waitMs: number
}

/** Long enough for a full-corpus replay to finish ahead of the next one queued for its lane. */
export const DEFAULT_WAIT_MS = 900_000

/** Who hears whom a caller waits for, and `undefined` once it stops waiting. */
export type Watch = (who: string | undefined) => Effect.Effect<void>

const unwatched: Watch = () => Effect.void

const POLL_MS = 500
export const REPORT_MS = 15_000

interface Holder {
  readonly pid: number
  readonly since: string
  readonly what: string
}

type Requirements = FileSystem.FileSystem | Roots.Service

/** The holder a lock file names; undefined where it names none readable. */
const holderOf = Effect.fn("Locks.holderOf")(function*(file: string) {
  const fs = yield* FileSystem.FileSystem
  const text = yield* fs.readFileString(file).pipe(Effect.orElseSucceed(() => undefined))
  if (text === undefined) return undefined
  const held = yield* Effect.try(() => JSON.parse(text) as Partial<Holder>).pipe(Effect.orElseSucceed(() => undefined))
  if (held === undefined || typeof held.pid !== "number") return undefined
  return { pid: held.pid, since: String(held.since), what: String(held.what) } satisfies Holder
})

/** Who holds `file` right now — a dead or unreadable holder swept out on the way past. */
const liveHolder = Effect.fn("Locks.liveHolder")(function*(file: string) {
  const fs = yield* FileSystem.FileSystem
  if (!(yield* fs.exists(file).pipe(Effect.orElseSucceed(() => false)))) return undefined
  const holder = yield* holderOf(file)
  if (holder !== undefined && (yield* Liveness.alive(holder.pid))) return holder
  yield* Console.warn(
    `[lock] breaking a stale lock on ${file}` +
      (holder === undefined ? " (unreadable)" : ` — pid ${holder.pid} is gone`)
  )
  yield* fs.remove(file, { force: true }).pipe(Effect.ignore)
  return undefined
})

const describe = (holder: Holder, what: string): string =>
  `pid ${holder.pid} has ${what} (${holder.what}, since ${holder.since})`

/** The lock on `file`, released by removing it. */
const takenOn = (fs: FileSystem.FileSystem, file: string): Taken => ({
  release: fs.remove(file, { force: true }).pipe(Effect.ignore)
})

/** Tells one process's drafts apart. */
let drafts = 0

/** A holder's draft beside the lock it is written for; never a `.json` a reader scan reads. */
const draftOf = (file: string): string => `${file}.${process.pid}.${++drafts}.draft`

/**
 * Take `file`, or say who has it. The holder is written whole to a draft of
 * its own, then hard-linked into place — the link fails where the file
 * exists — so two callers racing for one lock cannot both win it, and no
 * reader ever finds a holder's file half written.
 */
const claim = Effect.fn("Locks.claim")(function*(file: string, what: string, now: string) {
  const fs = yield* FileSystem.FileSystem
  yield* fs.makeDirectory(path.dirname(file), { recursive: true }).pipe(Effect.ignore)
  const draft = draftOf(file)
  const written = yield* fs.writeFileString(
    draft,
    `${JSON.stringify({ pid: process.pid, since: now, what } satisfies Holder)}\n`
  ).pipe(Effect.result)
  if (written._tag === "Failure") return { held: `${file} could not be locked: ${written.failure.message}` } as Taken | Held
  const linked = fs.link(draft, file).pipe(Effect.as(true), Effect.orElseSucceed(() => false))
  return yield* Effect.gen(function*() {
    if (yield* linked) return takenOn(fs, file) as Taken | Held
    const holder = yield* liveHolder(file)
    if (holder !== undefined) return { held: describe(holder, file) } satisfies Held
    if (yield* linked) return takenOn(fs, file)
    return { held: `${file} could not be locked` } satisfies Held
  }).pipe(Effect.ensuring(fs.remove(draft, { force: true }).pipe(Effect.ignore)))
})

/**
 * Attempt until nothing is in the way or the patience runs out, sleeping on
 * the fiber between attempts. Answers the blocker it gave up on, so a refusal
 * always names whom it waited for, and `undefined` on success.
 */
const waitFor = <R>(
  attempt: Effect.Effect<string | undefined, never, R>,
  patience: Patience,
  watch: Watch
): Effect.Effect<string | undefined, never, R> =>
  Effect.gen(function*() {
    const started = Date.now()
    let said = 0
    let last: string | undefined
    for (;;) {
      const who = yield* attempt
      if (who === undefined || Date.now() - started >= patience.waitMs) {
        if (last !== undefined) yield* watch(undefined)
        return who
      }
      if (who !== last) {
        last = who
        yield* watch(who)
      }
      const waited = Date.now() - started
      if (waited - said >= REPORT_MS) {
        said = waited
        yield* Console.log(`[lock] waiting for ${who}`)
      }
      yield* Effect.sleep(POLL_MS)
    }
  })

/** One readers-writer lock: where its holders live, and what a refusal calls it. */
interface Guarded {
  readonly dir: string
  readonly name: string
}

/** The box's lock under the deployment's lock root, so two checkouts share one box. */
const boxGuard: Effect.Effect<Guarded, Roots.RootsUnstated, Roots.Service> = Effect.map(
  Effect.flatMap(Roots.Service, (roots) => roots.locks),
  (root) => ({ dir: path.join(root, "box.lock.d"), name: "the box" })
)

const writerFile = (guarded: Guarded): string => path.join(guarded.dir, "writer.json")
const readerDir = (guarded: Guarded): string => path.join(guarded.dir, "readers")

/** The live readers of a guarded thing, the dead ones swept out on the way past. */
const readers = Effect.fn("Locks.readers")(function*(guarded: Guarded) {
  const fs = yield* FileSystem.FileSystem
  const entries = yield* fs.readDirectory(readerDir(guarded)).pipe(Effect.orElseSucceed(() => [] as Array<string>))
  const live: Array<Holder> = []
  for (const entry of entries.filter((name) => name.endsWith(".json"))) {
    const holder = yield* liveHolder(path.join(readerDir(guarded), entry))
    if (holder !== undefined) live.push(holder)
  }
  return live as ReadonlyArray<Holder>
})

/** The time a holder states it took the lock. */
const stamp = (): string => new Date().toISOString()

/**
 * Alone: the writer marker first — which stops the next reader — then the
 * readers already in are waited out. A wait that runs out gives the marker
 * back, so a refused caller leaves the thing exactly as it found it.
 */
const takeAlone = Effect.fn("Locks.takeAlone")(function*(
  guarded: Guarded,
  what: string,
  patience: Patience,
  watch: Watch
) {
  const now = stamp()
  let taken: Taken | undefined
  const blocked = yield* waitFor(
    Effect.map(claim(writerFile(guarded), what, now), (claimed) => {
      if ("held" in claimed) return claimed.held
      taken = claimed
      return undefined
    }),
    patience,
    watch
  )
  if (blocked !== undefined || taken === undefined) {
    return { held: blocked ?? `${guarded.name} could not be locked` } as Taken | Held
  }
  const marker = taken
  const busy = yield* waitFor(
    Effect.map(readers(guarded), (live) =>
      live.length === 0
        ? undefined
        : `${live.length} replay(s) still holding ${guarded.name}: ` +
          live.map((holder) => `pid ${holder.pid} (${holder.what})`).join(", ")),
    patience,
    watch
  )
  if (busy !== undefined) {
    yield* marker.release
    return { held: busy } as Taken | Held
  }
  return marker as Taken | Held
})

/** Tells one process's readers apart, for a caller that takes more than one. */
let seq = 0

/**
 * With the others: many at once, and none while somebody holds it alone. A
 * writer that takes it between the look and the claim WINS it — the reader
 * backs out rather than run under a thing being rewritten beneath it.
 */
const takeWithOthers = Effect.fn("Locks.takeWithOthers")(function*(
  guarded: Guarded,
  what: string,
  patience: Patience,
  watch: Watch
) {
  const now = stamp()
  const file = path.join(readerDir(guarded), `${process.pid}.${++seq}.json`)
  let taken: Taken | undefined
  const blocked = yield* waitFor(
    Effect.gen(function*() {
      const writer = yield* liveHolder(writerFile(guarded))
      if (writer !== undefined) return describe(writer, guarded.name)
      const claimed = yield* claim(file, what, now)
      if ("held" in claimed) return claimed.held
      const raced = yield* liveHolder(writerFile(guarded))
      if (raced === undefined) {
        taken = claimed
        return undefined
      }
      yield* claimed.release
      return describe(raced, guarded.name)
    }),
    patience,
    watch
  )
  if (blocked !== undefined || taken === undefined) {
    return { held: blocked ?? `${guarded.name} could not be read-locked` } as Taken | Held
  }
  return taken as Taken | Held
})

/**
 * The box held QUIET, for a replay whose expectations are wall-clock
 * deadlines: alone box-wide, so no neighbour's cells can time a real-clock
 * cell out.
 */
export const quiet = (
  what: string,
  patience: Patience,
  watch: Watch = unwatched
): Effect.Effect<Taken | Held, Roots.RootsUnstated, Requirements> =>
  Effect.flatMap(boxGuard, (guarded) => takeAlone(guarded, what, patience, watch))

/**
 * The box put under LOAD, for a replay that measures no wall time: many at
 * once, and none while a real-clock replay holds the box quiet.
 */
export const load = (
  what: string,
  patience: Patience,
  watch: Watch = unwatched
): Effect.Effect<Taken | Held, Roots.RootsUnstated, Requirements> =>
  Effect.flatMap(boxGuard, (guarded) => takeWithOthers(guarded, what, patience, watch))

/**
 * A box-wide singleton, by name: a build, or a lane whose SUT is one per
 * host. Named under the lock root, so two checkouts queue for the same ports
 * rather than fight over them.
 */
export const exclusive = (
  name: string,
  what: string,
  patience: Patience,
  watch: Watch = unwatched
): Effect.Effect<Taken | Held, Roots.RootsUnstated, Requirements> =>
  Effect.gen(function*() {
    const root = yield* Effect.flatMap(Roots.Service, (roots) => roots.locks)
    const file = path.join(root, `${name}.json`)
    const now = stamp()
    let taken: Taken | undefined
    const blocked = yield* waitFor(
      Effect.map(claim(file, what, now), (claimed) => {
        if ("held" in claimed) return claimed.held
        taken = claimed
        return undefined
      }),
      patience,
      watch
    )
    if (blocked !== undefined || taken === undefined) {
      return { held: blocked ?? `${file} could not be locked` } as Taken | Held
    }
    return taken as Taken | Held
  })

/** Whether a take came back as the lock rather than a refusal. */
export const isTaken = (outcome: Taken | Held): outcome is Taken => "release" in outcome
