/**
 * Capture ingest: a capture dropped on the viewer — one pcap, or the
 * consecutive files of one tap dropped together — saved where the pipeline
 * reads it (`_uploads`, one of the source roots) and handed to the source
 * command (its `add … --cut`), which makes an `ingest` JOB in the
 * store and launches it detached: the viewer inserts nothing itself and
 * holds no slot, so its restart loses nothing and two drops run at once. In
 * a workspace the drop is saved into its inbox instead and taken from there
 * by an inbox job (`inbox.ts`): a capture enters a workspace anonymised.
 *
 * The viewer never opens the capture. It only names it the way the pipeline
 * will — that name is the answer's link to the study view the run will fill —
 * and refuses at the door anything that is not a capture or carries a path.
 * Whether several files ARE one consecutive capture is the ingest's call
 * (`sipflow --contiguous`), reported in the job's log.
 */
import { Effect, Layer, Schema } from "effect"
import * as Context from "effect/Context"
import { FileSystem } from "effect/FileSystem"
import { Path } from "effect/Path"
import { InboxTake } from "@sip/lab-host"
import * as Discover from "@sip/corpus/ingest/discover"
import { spawn } from "node:child_process"
import { ViewerConfig } from "./config.ts"
import { Inbox } from "./inbox.ts"
import { SitePack } from "./site-pack.ts"

/** The page an inbox drop is followed on. */
const PCAPS = "/pcaps"

/** The capture forms the campaign reads; a drop of anything else is refused. */
export const PCAP_SUFFIXES = [".pcap", ".pcap.gz", ".pcapng", ".pcapng.gz"] as const

/**
 * A capture form, numbered as `tcpdump -C` numbers a ring or not: the rule
 * `@sip/corpus` `Discover.isRingMember` states, so a ring dropped
 * here is the ring `--set` takes.
 */
const isCapture = (filename: string): boolean =>
  /\.pcap(ng)?(\.gz)?\d*(\.gz)?$/i.test(filename)

/** One dropped file as the request carries it. */
export interface Part {
  /** The name the browser sent, which a hostile client may shape freely. */
  readonly filename: string
  /** Where the multipart part was persisted; valid for this request only. */
  readonly path: string
}

/** The dropped capture as the request carries it. */
export interface Upload {
  /** Its files; several are one capture. */
  readonly files: ReadonlyArray<Part>
  /**
   * The lane the dropper meant to replay on; an ingest job drives none, so a
   * lane here is answered in `detail` as the request to create after it.
   */
  readonly lane: string | undefined
}

export interface Accepted {
  /** The capture name the pipeline will use, and the study view keys on. */
  readonly capture: string
  readonly started: boolean
  readonly detail: string
  /** The page this capture will be readable at: its study view, or the Pcaps page for an inbox drop. */
  readonly study: string
  /** The drop went to the workspace's inbox, to be anonymised into its pcap root. */
  readonly inbox?: boolean
  /** The ingest job the source command made; absent where it made none. */
  readonly jobId?: number
}

/** How the source command's launch for one capture went: what the page reads until the jobs page replaces it. */
export interface Launch {
  readonly exit: number
  readonly startedAt: string
  readonly finishedAt: string
  readonly logTail: string
}

/** How the ingest for one capture stands: the launch as this server saw it. */
export interface State {
  /** Never held: an ingest is a job of the store, not a slot of this server. */
  readonly busyWith: string | undefined
  readonly last: Launch | undefined
  readonly detail: string
}

export class Refused extends Schema.TaggedError<Refused>()("Site.CaptureRefused", {
  filename: Schema.String,
  detail: Schema.String
}) {}

export interface Interface {
  readonly accept: (upload: Upload) => Effect.Effect<Accepted, Refused>
  readonly state: (capture: string) => Effect.Effect<State>
}

export class Service extends Context.Service<Service, Interface>()("@sip/site/Ingest") {}

/** The name the campaign gives one file: a trailing `.pcap` dropped, the rest folded. */
const stemOf = (filename: string): string =>
  (filename.endsWith(".pcap") ? filename.slice(0, -".pcap".length) : filename)
    .replace(/[^A-Za-z0-9._-]/g, "_")

/** Name order with numbers compared by value, as the campaign orders a set. */
const natural = new Intl.Collator("en", { numeric: true })

/** The files of a drop in the order the campaign reads them: ring order by name. */
export const ringOrder = <A extends { readonly filename: string }>(
  files: ReadonlyArray<A>
): ReadonlyArray<A> => [...files].sort((a, b) => natural.compare(a.filename, b.filename))

/**
 * The name the campaign gives a dropped capture, byte for byte as
 * `@sip/corpus` `Discover.captureOfSet` names it: one file is its basename with
 * a trailing `.pcap` dropped (`.pcap.gz` and the `.pcapng` forms keep theirs)
 * and every character outside the capture alphabet folded to `_`; several
 * files are the first in ring order with the count, `ring0_set3`.
 */
export const captureName = (filenames: ReadonlyArray<string>): string => {
  const ordered = ringOrder(filenames.map((filename) => ({ filename })))
  const first = stemOf(ordered[0]?.filename ?? "")
  return ordered.length === 1 ? first : `${first}_set${ordered.length}`
}

/** Why this name is not a capture the viewer will take; undefined = it is one. */
const refusalOf = (filename: string): string | undefined => {
  if (filename === "") return "a dropped file with no name is not a capture"
  if (filename.includes("/") || filename.includes("\\")) {
    return "a capture name carries no path separator"
  }
  return isCapture(filename)
    ? undefined
    : `only ${PCAP_SUFFIXES.join(", ")} (numbered as a ring or not) are captures`
}

/**
 * Why this drop is not a capture: no file, a file that is not one (named), or
 * two files of one name — they would land on one path and the set would read
 * one file twice.
 */
const refusal = (files: ReadonlyArray<Part>): string | undefined => {
  if (files.length === 0) return "a drop with no file is not a capture"
  const seen = new Set<string>()
  for (const one of files) {
    const why = refusalOf(one.filename)
    if (why !== undefined) return files.length === 1 ? why : `${one.filename}: ${why}`
    if (seen.has(one.filename)) return `${one.filename} is dropped twice; a set holds a file once`
    seen.add(one.filename)
  }
  return undefined
}

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const fs = yield* FileSystem
    const path = yield* Path
    const config = yield* ViewerConfig.Service
    const pack = yield* SitePack.Service
    const inbox = yield* Inbox.Service

    // The launches this server made, per capture: what the page polls for.
    const launched = new Map<string, Launch>()

    /**
     * The source command run to its end — it makes the job row, launches the
     * job detached and returns — with its output as the launch's log. A
     * command that cannot be spawned is an exit of -1 saying so.
     */
    const launch = Effect.fn("Ingest.launch")(function*(
      command: string,
      args: ReadonlyArray<string>,
      cwd: string | undefined
    ) {
      const startedAt = new Date().toISOString()
      const ran = yield* Effect.callback<{ readonly exit: number; readonly log: string }>((resume) => {
        let log = ""
        let settled = false
        const settle = (exit: number, said?: string): void => {
          if (settled) return
          settled = true
          if (said !== undefined) log += (log === "" ? "" : "\n") + said
          resume(Effect.succeed({ exit, log }))
        }
        const child = spawn(command, [...args], { cwd, stdio: ["ignore", "pipe", "pipe"] })
        const append = (chunk: Buffer): void => {
          log += chunk.toString("utf8")
        }
        child.stdout.on("data", append)
        child.stderr.on("data", append)
        child.on("error", (error) => settle(-1, `spawn failed: ${error.message}`))
        child.on("close", (code) => settle(code ?? -1))
      })
      return { ...ran, startedAt, finishedAt: new Date().toISOString() }
    })

    /** The job id the source command printed (`job N: …`), where it printed one. */
    const jobIdOf = (log: string): number | undefined => {
      const match = /^job (\d+):/m.exec(log)
      return match === null ? undefined : Number(match[1])
    }

    /**
     * The multipart part is persisted in the OS temp directory, which need not
     * sit on the artifact store's filesystem: a cross-device rename fails and a
     * copy stands in for it. A name already in the store is overwritten — the
     * same capture dropped twice is one capture.
     */
    const save = Effect.fn("Ingest.save")(function*(from: string, to: string) {
      yield* fs.makeDirectory(path.dirname(to), { recursive: true })
      yield* fs.rename(from, to).pipe(
        Effect.catch(() =>
          fs.copyFile(from, to).pipe(Effect.andThen(fs.remove(from).pipe(Effect.ignore)))
        )
      )
    })

    /**
     * Every file of a drop saved, or none: a set half in the store would be a
     * ring with holes the next drop of the same names could complete by
     * accident.
     */
    const saveAll = Effect.fn("Ingest.saveAll")(function*(
      files: ReadonlyArray<Part>,
      targets: ReadonlyArray<string>
    ) {
      const saved: Array<string> = []
      yield* Effect.forEach(files, (one, i) =>
        save(one.path, targets[i]!).pipe(Effect.tap(() => Effect.sync(() => saved.push(targets[i]!)))), {
        discard: true
      }).pipe(
        Effect.onError(() =>
          Effect.forEach(saved, (target) => fs.remove(target).pipe(Effect.ignore), { discard: true })
        )
      )
    })

    /**
     * A workspace's drop: saved into its inbox and taken from there at once
     * by an inbox job, its files as one capture; it reaches the population
     * only when the Pcaps page replays its anonymised copy.
     */
    const intoInbox = Effect.fn("Ingest.intoInbox")(function*(dir: string, files: ReadonlyArray<Part>) {
      const filename = files.map((one) => one.filename).join(", ")
      const copy = InboxTake.copyName(path.basename(files[0]!.filename))
      const capture = Discover.captureOf(copy)
      const targets = files.map((one) => path.join(dir, path.basename(one.filename)))
      const failed = yield* saveAll(files, targets).pipe(
        Effect.map(() => undefined as string | undefined),
        Effect.catch((cause) => Effect.succeed(String(cause)))
      )
      if (failed !== undefined) {
        return { capture, started: false, detail: `could not save ${filename} under ${dir}: ${failed}`, study: PCAPS, inbox: true } satisfies Accepted
      }
      const taken = yield* inbox.take(targets).pipe(Effect.catch((refused) => Effect.succeed(refused)))
      if (taken instanceof Inbox.Refused) {
        return { capture, started: false, detail: taken.detail, study: PCAPS, inbox: true } satisfies Accepted
      }
      return {
        capture,
        started: true,
        detail: `job ${taken.id}: ${taken.label}`,
        study: PCAPS,
        inbox: true,
        jobId: taken.id
      } satisfies Accepted
    })

    const accept = Effect.fn("Ingest.accept")(function*(upload: Upload) {
      const filename = upload.files.map((one) => one.filename).join(", ")
      const detail = refusal(upload.files)
      if (detail !== undefined) {
        return yield* new Refused({ filename, detail })
      }
      const triage = config.triage
      if (triage === undefined) {
        return yield* new Refused({
          filename,
          detail: `no triage block in ${config.configPath}: there is no artifact store to drop into`
        })
      }
      const files = ringOrder(upload.files)
      const workspace = config.workspace
      if (workspace !== undefined) return yield* intoInbox(workspace.inbox, files)
      const capture = captureName(files.map((one) => one.filename))
      const targets = files.map((one) => path.join(triage.uploads, path.basename(one.filename)))
      // An unwritable store is the deployment's fault, not the dropper's: it
      // is reported in place, on the drop, rather than as a failed request.
      const failed = yield* saveAll(files, targets).pipe(
        Effect.map(() => undefined as string | undefined),
        Effect.catch((cause) => Effect.succeed(String(cause)))
      )
      if (failed !== undefined) {
        return {
          capture,
          started: false,
          detail: `could not save ${filename} under ${triage.uploads}: ${failed}`,
          study: `/triage/${capture}`
        } satisfies Accepted
      }
      const args = [
        ...(triage.ingest.args ?? []),
        ...(targets.length === 1 ? [targets[0]!] : targets.flatMap((target) => ["--set", target])),
        "--cut"
      ]
      const ran = yield* launch(triage.ingest.command, args, triage.ingest.cwd)
      launched.set(capture, { exit: ran.exit, startedAt: ran.startedAt, finishedAt: ran.finishedAt, logTail: ran.log })
      const jobId = ran.exit === 0 ? jobIdOf(ran.log) : undefined
      const lane = upload.lane
      const said = ran.exit === 0
        ? (jobId === undefined ? ran.log.trim() : `job ${jobId}: ingest + cut of ${capture} launched`) +
          (lane === undefined ? "" : `; a replay on ${lane} is a request to create once it is done`)
        : `${triage.ingest.command} exited ${ran.exit}: ${ran.log.trim()}`
      return {
        capture,
        started: ran.exit === 0,
        detail: said,
        study: `/triage/${capture}`,
        ...(jobId === undefined ? {} : { jobId })
      } satisfies Accepted
    })

    const state = Effect.fn("Ingest.state")(function*(capture: string) {
      const last = yield* Effect.sync(() => launched.get(capture))
      return {
        busyWith: undefined,
        last,
        detail: last === undefined ? "no drop of this capture since the server started" : `launched as a job: \`${pack.jobCommand} list\``
      } satisfies State
    })

    return Service.of({ accept, state })
  })
)

export * as Ingest from "./ingest.ts"
