/**
 * The lane's BATCH PROCESS: one `<interpreter> batch` per batch on a hosted
 * lane, serving the backend the SUT calls at the lane's backend address and
 * running every cell of the batch as a task of its own (`@sip/contracts`
 * `batch.ts`, the pack's parts registered in its interpreter). What the SUT
 * records is its collectors', around the batch.
 *
 * A cell reaches it through the interpreter seam the driver already runs a
 * cell through: `ReplayCli.run(specPath)` writes the run-spec's path on the
 * process's stdin and resolves on the cell's line on its stdout, carrying the
 * `replay <run-spec>` exit vocabulary — so `runCell` reads a cell the batch ran
 * exactly as it reads one it exec'd. `end` closes stdin and resolves on the
 * batch's own gate (the SUT's reap, in the process); `kill` is the stop's
 * SIGTERM, which the process answers by exiting, and resolves once it closed
 * (a SIGKILL past the grace). A protocol line that does not
 * decode fails what it names. A run-spec already running in the batch is
 * refused, never waited on twice: the process answers each spec by its
 * path. The end line is waited for at most the pack's `endWithin`, the process killed
 * past it, and the host says whether it came ({@link Host.judged}). While a
 * cell line or the end line is awaited, what is awaited is printed every
 * {@link EVERY_MS}. The memory box is the batch's, sized by its cells, the
 * interpreter run in its pack's wrapper. The ready line and the backend
 * summary of the end line (its `bl` key) are printed as `[batch]` lines.
 *
 * An `inject` line a cell's run asks is performed by the injector that run
 * was given, one ask at a time per cell, and answered on stdin echoing its
 * ask id, spec and node: `injected` once it did its work, `refused` with its
 * words; a cell run with no injector has each inject refused ({@link NO_STEP}).
 * An `inject` line that does not decode but names its ask id, spec and node is
 * refused with why, and its cell, once its line came, is an environment
 * failure naming why; one naming less fails its cell at once.
 *
 * The simulated lane has no batch process: its cell builds its backend in-process.
 */
import { Batch } from "@sip/contracts"
import type { BatchHost, ContextsReport, InjectAsk, Injector } from "@sip/corpus/replay/batch-protocol"
import type { LaneView } from "@sip/rig"
import { ReplayCli } from "@sip/toolchain"
import * as Effect from "effect/Effect"
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process"
import * as FileSystem from "effect/FileSystem"
import * as Queue from "effect/Queue"
import * as Result from "effect/Result"
import * as Schema from "effect/Schema"
import * as path from "node:path"
import { boxVariables } from "./interpreter-env.js"
import type { LabPack } from "./lab-pack.js"
import { noRoutingPath } from "./lane-checks.js"
import * as UserSession from "./user-session.js"

/** How long the process gets to serve its backend before the batch gives up on it. */
const READY_MS = 20_000

/** How long a killed process gets to exit on its SIGTERM before it is SIGKILLed. */
const KILL_GRACE_MS = 10_000

/**
 * How a cell line opens when the batch found its SUT never called the backend
 * it serves (pivot-replay's `src/batch/wiring.rs`): every later cell
 * would fail the same way, so none is sent and each answers this line.
 */
export const MISWIRED = "environment: lane wiring:"

/** How often what the batch is awaited for is printed. */
export const EVERY_MS = 30_000

/** The refusal of an inject asked by a cell run with no injector. */
export const NO_STEP = "the harness serves no operation step for this cell"

/** How {@link attach} bounds the process, in milliseconds: its readiness, its end line, how often a wait is printed. */
export interface AttachOptions {
  readonly readyWithin?: number
  /** How long the end line may take once stdin closed: the pack's interpreter's end bound and its end's work. */
  readonly endWithin: number
  readonly every?: number
  /** Where a lasting wait is printed; stderr by default. */
  readonly say?: (line: string) => void
  /** What the backend served, as the `[batch]` line at the end prints it. */
  readonly servedLine: (served: Served) => string
}

/** The batch process as the harness holds it: whether its own end ran, beside the protocol. */
export interface Host extends BatchHost {
  /**
   * Whether the end line came: the batch's end gates and its end's work ran.
   * A batch ended otherwise (its bound, its death) judged none of its cells'
   * end checks.
   */
  readonly judged: () => boolean
}

/** What a batch process is stood with. */
export interface Request {
  /** Where the batch's scratch goes: the spec, the backend's facts. */
  readonly under: string
  /** The one directory holding a row for every number the batch's cells dial. */
  readonly ledgerDir?: string
  /** The memory box of ONE cell (`--memory-max`); the batch's is this times `cells`. */
  readonly memoryMax: string
  /** How many cells the batch runs at once. */
  readonly cells: number
}

/**
 * What a pack's interpreter reads of a hosted lane and of one batch beside
 * the batch-spec's own fields (`@sip/contracts` `Batch.BatchSpec`): the
 * backend address `backend` it serves or relays at, and whatever else its
 * parts and the request state.
 */
export type Wiring<L extends LaneView.Hosted, R extends Request> = (
  lane: L,
  backend: string,
  request: R
) => Readonly<Record<string, unknown>>

/** What a pack stands its batch process with. */
export interface BatchPack<L extends LaneView.View, R extends Request> {
  readonly lab: Pick<LabPack, "interpreter" | "variables">
  readonly wiring: Wiring<Extract<L, { readonly kind: "hosted" }>, R>
  /** The end line's bound ({@link AttachOptions.endWithin}). */
  readonly endWithin: number
  /** What the backend served, in the pack's words ({@link AttachOptions.servedLine}). */
  readonly servedLine: (served: Served) => string
}

/** The batch-spec the process reads: the lane, its `wiring`, the scratch and the ledger. */
export const batchSpec = (
  lane: LaneView.Hosted,
  wiring: Readonly<Record<string, unknown>>,
  workDir: string,
  ledgerDir: string | undefined
) => ({
  lane: lane.id,
  ...wiring,
  work_dir: workDir,
  ...(ledgerDir === undefined ? {} : { ledger_dir: ledgerDir })
})

/** `one` cell's memory box times `cells`, in MiB; a size it cannot read passes as given. */
export const scaled = (one: string, cells: number): string => {
  const match = /^(\d+(?:\.\d+)?)([KMGT]?)$/i.exec(one.trim())
  if (match === null) return one
  const unit = { "": 1 / 2 ** 20, K: 1 / 1024, M: 1, G: 1024, T: 1024 * 1024 }[match[2]!.toUpperCase()]!
  return `${Math.ceil(Number(match[1]) * unit * Math.max(cells, 1))}M`
}

/**
 * A cell line's report in the interpreter seam's vocabulary. The lines the
 * cell wrote on stderr are its stderr; a cell that did not pass opens it with
 * its summary, the one line stating its verdict, cleanup and audit, so what a
 * failing cell keeps of its interpreter is never empty.
 */
export const reportOf = (exit: number, summary: string, stderr = ""): ReplayCli.RunReport => ({
  outcome: ReplayCli.outcomeOf(exit),
  exitCode: exit,
  stdout: exit <= 1 ? summary : "",
  stderr: exit === 0 ? stderr : `${summary}\n${stderr}`
})

/** What the end line states the backend served, its `bl` key (`Served` in pivot-replay's `src/batch/served.rs`). */
const EndServed = Schema.Struct({
  end: Schema.Struct({
    bl: Schema.Struct({
      served: Schema.Number,
      process_us: Schema.NullOr(Schema.Struct({ p50: Schema.Number, p99: Schema.Number, max: Schema.Number }))
    })
  })
})

/** What the backend served over a batch: its requests, and its process time in µs where the end line states one. */
export interface Served {
  readonly served: number
  readonly processUs: { readonly p50: number; readonly p99: number; readonly max: number } | null
}

/** What the backend served, as an end line states it under its `bl` key; none where the line states none. */
export const servedOf = (line: string): Served | undefined => {
  const decoded = Schema.decodeUnknownResult(Schema.fromJsonString(EndServed))(line)
  if (decoded._tag !== "Success") return undefined
  const { served, process_us } = decoded.success.end.bl
  return { served, processUs: process_us }
}

/**
 * Stand the lane's batch process, or answer `undefined` for the simulated
 * lane, which has none. A lane whose harness is not on the SUT's routing path
 * stands none and fails, naming why. Resolves once the process serves the
 * backend.
 */
export const stand = <L extends LaneView.View, R extends Request>(pack: BatchPack<L, R>, found: L, request: R) =>
  Effect.gen(function*() {
    if (found.kind === "simulated") return undefined
    const hosted = found as Extract<L, { readonly kind: "hosted" }>
    const backend = hosted.backendListen
    if (backend === undefined) return yield* Effect.fail(new Error(noRoutingPath(hosted)))
    const lane = hosted.id
    const fs = yield* FileSystem.FileSystem
    yield* fs.makeDirectory(request.under, { recursive: true })
    const workDir = yield* fs.makeTempDirectory({ directory: request.under, prefix: "batch-" })
    const specPath = path.join(workDir, "batch-spec.json")
    const spec = batchSpec(hosted, pack.wiring(hosted, backend, request), workDir, request.ledgerDir)
    yield* fs.writeFileString(specPath, JSON.stringify(spec, null, 2) + "\n")
    const session = yield* UserSession.answers(pack.lab.variables.userSession)
    const child = yield* Effect.sync(() =>
      spawn(pack.lab.interpreter.wrapper, ["batch", specPath], {
        env: { ...process.env, ...boxVariables(pack.lab, scaled(request.memoryMax, request.cells), session) },
        stdio: ["pipe", "pipe", "pipe"]
      })
    )
    const host = attach(child, lane, { endWithin: pack.endWithin, servedLine: pack.servedLine })
    yield* host.ready
    console.log(`[batch] ${lane}: one replay batch process, scratch ${workDir}`)
    return host.host
  }).pipe(Effect.withSpan("ReplayBatch.stand"))

/**
 * A failed write to the batch process's stdin, said on stderr: one that died
 * before its `close` (EPIPE) is no death of the driver, since `close` settles
 * every cell still waiting.
 */
const sayStdinError = (e: Error) => void process.stderr.write(`[replay batch] stdin: ${e.message}\n`)

/**
 * The protocol over a spawned `<interpreter> batch`. A `ready` that fails —
 * the process exited, stayed silent past its bound, or the stand was
 * interrupted — has killed the process by the time it does, so a stand that
 * hands no host over leaves no process holding the lane's addresses.
 */
export const attach = (child: ChildProcessWithoutNullStreams, lane: string, options: AttachOptions) => {
  const pending = new Map<string, (report: ReplayCli.RunReport) => void>()
  /** Per running cell given an injector, where its asks go. */
  const injectors = new Map<string, (ask: InjectAsk) => void>()
  /** Per running cell an inject line of which did not decode, why: the cell fails once its own line came. */
  const unreadAsks = new Map<string, string>()
  let exit: string | undefined
  let ended: ((dirty: string | undefined) => void) | undefined
  let endLine:
    | {
      dirty: string | undefined
      contexts: ContextsReport | undefined
      gates: ReadonlyArray<Batch.Gate>
      malformed?: true
    }
    | undefined
  let readied: ((error: Error | undefined) => void) | undefined
  let isReady = false
  /** The batch's lane-wiring error, once a cell line named it: no later cell is sent. */
  let miswired: string | undefined
  /** Why readiness failed before anyone waited on it; a later wait fails with it at once. */
  let unready: Error | undefined
  /** When stdin was closed for the end line, while it is awaited. */
  let ending: number | undefined
  const say = options.say ?? ((line: string) => void process.stderr.write(`${line}\n`))
  const startedAt = Date.now()
  const every = options.every ?? EVERY_MS
  const ticker = setInterval(() => {
    const now = Date.now()
    if (ending !== undefined) {
      say(`[batch] ${lane}: waiting ${Math.round((now - ending) / 1000)} s for the end line`)
    } else if (pending.size > 0) {
      const specs = [...pending.keys()].map((spec) => path.basename(spec))
      const named = specs.length > 5 ? `${specs.slice(0, 5).join(", ")} and ${specs.length - 5} more` : specs.join(", ")
      say(`[batch] ${lane}: ${Math.round((now - startedAt) / 1000)} s in, waiting for ${specs.length} cell line(s): ${named}`)
    }
  }, every)
  ticker.unref()

  const lost = (why: string) => {
    for (const [spec, resume] of pending) resume(reportOf(3, `environment: ${why} before ${spec} ended`))
    pending.clear()
    unreadAsks.clear()
  }

  let buffered = ""
  child.stdout.on("data", (chunk: Buffer) => {
    buffered += chunk.toString()
    let at: number
    while ((at = buffered.indexOf("\n")) >= 0) {
      const line = buffered.slice(0, at)
      buffered = buffered.slice(at + 1)
      onLine(line)
    }
  })
  child.stderr.on("data", (chunk: Buffer) => process.stderr.write(`[replay batch] ${chunk.toString()}`))
  child.stdin.on("error", sayStdinError)

  /**
   * An inject line that does not decode: refused with `why` wherever its ask
   * reads. The running cell it names fails once its line came, so the call it
   * holds ends first; one naming no running cell fails no other where its ask
   * read, every one where nothing of it does.
   */
  const malformedInject = (read: Extract<Batch.Read, { readonly _tag: "malformed" }>, why: string) => {
    if (read.asked !== undefined) answer(Batch.refusedLine(read.asked, why))
    const spec = read.asked?.spec ?? read.spec
    if (spec !== undefined && pending.has(spec)) return void unreadAsks.set(spec, why)
    if (read.asked === undefined) lost(why)
  }

  /**
   * A protocol line that does not decode: the cell, the end or the readiness it
   * names fails, never waits; an inject as {@link malformedInject} says.
   */
  const malformed = (read: Extract<Batch.Read, { readonly _tag: "malformed" }>) => {
    const why = `the ${lane} batch process wrote ${read.key === "inject" ? "an" : "a"} ${read.key} line that does not decode: ${read.why}`
    if (read.key === "inject") return malformedInject(read, why)
    if (read.key === "cell") {
      const resume = read.spec === undefined ? undefined : pending.get(read.spec)
      if (resume === undefined) return lost(why)
      pending.delete(read.spec!)
      return resume(reportOf(3, `environment: ${read.spec}: ${why}`))
    }
    if (read.key === "end") {
      endLine = { dirty: why, contexts: undefined, gates: [], malformed: true }
      return ended?.(why)
    }
    unready = new Error(why)
    readied?.(unready)
  }

  const onLine = (line: string) => {
    const read = Batch.readLine(line)
    if (read._tag === "talk") {
      if (line.trim() !== "") process.stderr.write(`[replay batch] ${line}\n`)
      return
    }
    if (read._tag === "malformed") return malformed(read)
    const parsed = read.line
    if (parsed.ready !== undefined) {
      isReady = true
      console.log(`[batch] ${lane}: ${JSON.stringify(parsed.ready)}`)
      readied?.(undefined)
    } else if (parsed.cell !== undefined) {
      const cell = parsed.cell
      const resume = pending.get(cell.spec)
      pending.delete(cell.spec)
      if (miswired === undefined && cell.summary.startsWith(MISWIRED)) {
        miswired = cell.summary
        console.error(`[batch] ${lane}: ${miswired.slice("environment: ".length)}; no further cell is sent`)
      }
      const unread = unreadAsks.get(cell.spec)
      unreadAsks.delete(cell.spec)
      resume?.(
        unread === undefined
          ? reportOf(cell.exit, cell.summary, cell.stderr ?? "")
          : reportOf(3, `environment: ${cell.spec}: ${unread}`, `the cell ended: ${cell.summary}\n${cell.stderr ?? ""}`)
      )
    } else if (parsed.end !== undefined) {
      const served = servedOf(line)
      if (served !== undefined) console.log(`[batch] ${lane}: ${options.servedLine(served)}`)
      endLine = {
        dirty: parsed.end.dirty ?? undefined,
        contexts: parsed.end.contexts ?? undefined,
        gates: parsed.end.gates ?? []
      }
      ended?.(endLine.dirty)
    } else if (parsed.inject !== undefined) {
      const serve = injectors.get(parsed.inject.spec)
      if (serve === undefined) answer(Batch.refusedLine(parsed.inject, NO_STEP))
      else serve(parsed.inject)
    }
  }

  /** `line` written on the process's stdin, where it still reads it. */
  const answer = (line: string) => {
    if (exit === undefined && child.stdin.writable) child.stdin.write(`${line}\n`)
  }

  /** `ask` performed by `injector` and answered: injected once it succeeded, refused with its words where it failed. */
  const answered = (ask: InjectAsk, injector: Injector) =>
    Effect.flatMap(Effect.result(injector(ask)), (done) =>
      Effect.sync(() => answer(Result.isSuccess(done) ? Batch.injectedLine(ask) : Batch.refusedLine(ask, done.failure))))

  /** The process gone, on its `close` rather than its `exit`: its last lines are read before it counts as gone. */
  const onClose = (code: number | null, signal: NodeJS.Signals | null) => {
    clearInterval(ticker)
    exit = `the ${lane} batch process exited ${signal ?? code}`
    lost(exit)
    if (!isReady) readied?.(new Error(`${exit} before it served the backend`))
    if (endLine === undefined) ended?.(exit)
  }
  child.on("close", onClose)
  child.on("error", (e) => {
    clearInterval(ticker)
    exit = `the ${lane} batch process could not run: ${e.message}`
    lost(exit)
    readied?.(e)
    ended?.(exit)
  })

  /** SIGTERM, SIGKILL past the grace; resolves once the process closed. A process already gone is left. */
  const kill = Effect.callback<void>((resume) => {
    if (exit !== undefined) return resume(Effect.void)
    const forced = setTimeout(() => void child.kill("SIGKILL"), KILL_GRACE_MS)
    child.once("close", () => {
      clearTimeout(forced)
      resume(Effect.void)
    })
    child.kill("SIGTERM")
  })

  const awaitReady = Effect.callback<void, Error>((resume) => {
    if (isReady) return resume(Effect.void)
    if (unready !== undefined) return resume(Effect.fail(unready))
    if (exit !== undefined) return resume(Effect.fail(new Error(exit)))
    const timer = setTimeout(
      () => resume(Effect.fail(new Error(`the ${lane} batch process never served its backend`))),
      options.readyWithin ?? READY_MS
    )
    readied = (error) => {
      clearTimeout(timer)
      resume(error === undefined ? Effect.void : Effect.fail(error))
    }
    return Effect.sync(() => clearTimeout(timer))
  })
  const ready = awaitReady.pipe(Effect.onError(() => kill))

  /** An interrupted run's cell `specPath`, no longer waited on: it runs on in the process, and the batch's end or kill settles it. */
  const unwaited = (specPath: string) =>
    Effect.sync(() => {
      pending.delete(specPath)
      unreadAsks.delete(specPath)
    })

  /**
   * One cell given `injector`: its asks queued while it runs and served one at
   * a time, the asks no longer taken once its cell line came or it was
   * interrupted.
   */
  const injected = (specPath: string, injector: Injector): Effect.Effect<ReplayCli.RunReport> =>
    Effect.scoped(Effect.gen(function*() {
      const asks = yield* Queue.unbounded<InjectAsk>()
      yield* Effect.acquireRelease(
        Effect.sync(() => void injectors.set(specPath, (ask) => void Queue.offerUnsafe(asks, ask))),
        () => Effect.sync(() => void injectors.delete(specPath))
      )
      yield* Effect.forkScoped(Effect.forever(Effect.flatMap(Queue.take(asks), (ask) => answered(ask, injector))))
      return yield* cellRun(specPath)
    }))

  const run = (specPath: string, injector?: Injector): Effect.Effect<ReplayCli.RunReport> =>
    injector === undefined ? cellRun(specPath) : injected(specPath, injector)

  const cellRun = (specPath: string): Effect.Effect<ReplayCli.RunReport> =>
    Effect.callback<ReplayCli.RunReport>((resume) => {
      if (exit !== undefined) return resume(Effect.succeed(reportOf(3, `environment: ${exit}`)))
      if (miswired !== undefined) return resume(Effect.succeed(reportOf(3, miswired)))
      if (pending.has(specPath)) {
        return resume(Effect.succeed(reportOf(3, `environment: ${specPath} is already running in this batch`)))
      }
      pending.set(specPath, (report) => resume(Effect.succeed(report)))
      child.stdin.write(`${specPath}\n`)
      return unwaited(specPath)
    })

  const endWithin = options.endWithin
  const awaitEnd = Effect.callback<string | undefined>((resume) => {
    if (endLine !== undefined) return resume(Effect.succeed(endLine.dirty))
    if (exit !== undefined) return resume(Effect.succeed(exit))
    const timer = setTimeout(() => {
      ending = undefined
      ended = undefined
      resume(Effect.succeed(`the ${lane} batch process wrote no end line within ${Math.round(endWithin / 1000)} s`))
    }, endWithin)
    ended = (dirty) => {
      clearTimeout(timer)
      ending = undefined
      resume(Effect.succeed(dirty))
    }
    ending = Date.now()
    child.stdin.end()
    return Effect.sync(() => {
      clearTimeout(timer)
      ending = undefined
    })
  })
  /** The batch's end, its dirt; a process silent past the end bound is stopped, so the lab is left as found and its addresses free. */
  const end = Effect.tap(awaitEnd, () => (endLine === undefined && exit === undefined ? kill : Effect.void))

  const host: Host = {
    run,
    died: () => exit,
    end,
    contexts: () => endLine?.contexts,
    gates: () => endLine?.gates ?? [],
    kill,
    judged: () => endLine !== undefined && !endLine.malformed
  }
  return { ready, host }
}
