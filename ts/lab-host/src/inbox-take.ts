/**
 * One INBOX TAKE: the files of one capture in a workspace's `inbox/` landed in
 * `pcaps/` at the request's level (`inbox-level.ts`), the capture's ONE
 * anonymisation. At an anonymised level the anonymiser the request names
 * reads the raw capture once and writes a verified copy and its flows
 * document (it refuses to write a copy anything of the raw capture survives
 * in); at `none` the files land unchanged. Either way a take marker
 * (`inbox-marker.ts`) beside the landed file says so, and the ingest anonymises
 * nothing again.
 *
 * The copy is `<stem>.anon.pcap`, its seed `.seeds/<stem>.seed` (0600), so the
 * same raw capture taken again rewrites the same bytes and lands as the copy
 * already there. A copy of that name holding other bytes is another capture:
 * the new one takes the next free name, `<stem>-2.anon.pcap`, `-3`…, under a
 * fresh seed of its own; a copy in `pcaps/` is never replaced. At `none` the
 * file keeps its name, `<stem>-2…` where another capture holds it. Each
 * name a take may land under is claimed first (`pcaps/.<copy>.taking`,
 * created exclusively, naming its holder's pid), so two takes never land one
 * name. A ring lands only anonymised, as one copy; at `none` it is refused.
 * The landed file is digested before it lands, and its marker is written
 * before it is renamed in ({@link commit}).
 *
 * Only files directly under the inbox are taken. They leave it only once the
 * copy is in `pcaps/`, and only where each still stands as the take found it:
 * a file copied over meanwhile is another capture and stays. Whatever the
 * anonymiser leaves of the raw capture (its raw model, its rejected
 * datagrams) is written under the take's own work directory (0700), never in
 * the inbox, and goes with it, however the take ends. A take that fails keeps
 * every raw file in the inbox and says why.
 */
import type { AnonymiseLevel } from "@sip/workspace"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Schema from "effect/Schema"
import * as Scope from "effect/Scope"
import { spawn } from "node:child_process"
import * as nodeFs from "node:fs"
import * as path from "node:path"
import { sidecarOf } from "./inbox-level.js"
import * as InboxMarker from "./inbox-marker.js"
import { stemOf } from "./inbox-scan.js"


/** How long a take waits between two looks at a name another live take holds. */
const CLAIM_POLL_MS = 1000

/** One raw file as the take was asked for it. */
export interface File {
  /** Absolute. */
  readonly path: string
  readonly size: number
  readonly mtimeMs: number
}

/** The workspace places a take reads and writes (`workspace.ts` `Layout`). */
export interface Places {
  readonly inbox: string
  readonly pcaps: string
  readonly seeds: string
}

export interface Request {
  /** The files of ONE capture, in the order its reader takes them. */
  readonly files: ReadonlyArray<File>
  readonly places: Places
  /** A directory of the take's own: the anonymiser's output and the raw side files, removed at the end. */
  readonly work: string
  /**
   * The anonymiser's command: it takes the capture's files (`--set` before
   * each of several), the out directory, `--seed`, `--raw-dir` and `--level`,
   * writes `<stem>.anon.pcap` and `<stem>.anon.flows.json` there, and exits
   * non-zero where the copy does not verify.
   */
  readonly anonymiser: string
  /** The level the capture is taken at (`inbox-level.ts` `levelOf`). */
  readonly level: AnonymiseLevel.Level
}

/** What a take came to. */
export interface Taken {
  /** The landed capture's (first) file, in `pcaps/`. */
  readonly pcap: string
  readonly level: AnonymiseLevel.Level
  /** The seed of an anonymised copy; absent at `none`. */
  readonly seed: string | undefined
  /** The copy was there already, byte for byte: the same capture taken again. */
  readonly already: boolean
  /** The raw files removed from the inbox. */
  readonly removed: ReadonlyArray<string>
  /** The raw files left in the inbox: each changed since the take found it. */
  readonly kept: ReadonlyArray<string>
}

/** The take did not land a copy; the raw files stay in the inbox. */
export class Refused extends Schema.TaggedError<Refused>()("InboxTake.Refused", { reason: Schema.String }) {
  override get message(): string {
    return this.reason
  }
}

/** The copy name of the capture whose first file is `name`: its stem. */
export const copyStem = (name: string): string => stemOf(name)

/** The file the copy of stem `stem` is written as. */
export const copyFile = (stem: string): string => `${stem}.anon.pcap`

/** The flows document the anonymiser writes beside the copy of stem `stem`. */
const copyDocument = (stem: string): string => `${stem}.anon.flows.json`

/**
 * The file the capture starting at `name` is first landed as at `level`: its
 * anonymised copy, or at `none` the file itself.
 */
export const copyName = (name: string, level: AnonymiseLevel.Level): string =>
  level === "none" ? name : copyFile(copyStem(name))

/** The seed kept for the copy of stem `stem`: dots written `_`, as the anonymiser names a seed. */
export const seedFile = (stem: string): string => `${stem.replaceAll(".", "_")}.seed`

/** The `n`-th name a copy of stem `stem` may take: the stem itself, then `<stem>-2`, `-3`… */
const nth = (stem: string, n: number): string => (n === 1 ? stem : `${stem}-${n}`)

/** The claim on one copy name: a dotfile beside it, which no listing of `pcaps/` reads. */
const claimFile = (pcaps: string, stem: string): string => path.join(pcaps, `.${copyFile(stem)}.taking`)

/**
 * The anonymiser run to its end, its output into this process's own (the job
 * log); its exit code. An interrupted take stops it.
 */
const run = (command: string, args: ReadonlyArray<string>) =>
  Effect.callback<number>((resume) => {
    const child = spawn(command, [...args], { stdio: ["ignore", "inherit", "inherit"] })
    child.on("error", (error) => {
      process.stderr.write(`[inbox] ${command} did not start: ${error.message}\n`)
      resume(Effect.succeed(-1))
    })
    child.on("close", (code) => resume(Effect.succeed(code ?? -1)))
    return Effect.sync(() => {
      if (child.exitCode === null && child.signalCode === null) child.kill("SIGTERM")
    })
  })

/**
 * `from` moved to `to`: a rename, or across filesystems a copy to a dotfile
 * beside `to` then a rename, so `to` is never seen half written.
 */
const move = Effect.fn("InboxTake.move")(function*(from: string, to: string) {
  const fs = yield* FileSystem.FileSystem
  const renamed = yield* fs.rename(from, to).pipe(Effect.as(true), Effect.orElseSucceed(() => false))
  if (renamed) return
  const landing = path.join(path.dirname(to), `.${path.basename(to)}.landing`)
  yield* fs.copyFile(from, landing)
  yield* fs.rename(landing, to)
  yield* fs.remove(from)
})

/**
 * Whether `file` still stands at the size and modification time the take was
 * asked for it at, the time within a millisecond: a `Date` holds no finer, a
 * recorded `mtimeMs` may.
 */
const unchanged = Effect.fn("InboxTake.unchanged")(function*(file: File) {
  const fs = yield* FileSystem.FileSystem
  const info = yield* fs.stat(file.path).pipe(Effect.option)
  if (info._tag === "None") return false
  const mtime = info.value.mtime._tag === "Some" ? info.value.mtime.value.getTime() : 0
  return Number(info.value.size) === file.size && Math.abs(mtime - file.mtimeMs) < 1
})

/** The SHA-256 of a file; one that does not read refuses the take. */
const digest = (file: string) =>
  InboxMarker.digest(file).pipe(Effect.mapError((unreadable) => new Refused({ reason: unreadable.reason })))

/** Whether two files hold the same bytes. */
const sameBytes = Effect.fn("InboxTake.sameBytes")(function*(a: string, b: string) {
  const fs = yield* FileSystem.FileSystem
  const [one, two] = [yield* fs.stat(a), yield* fs.stat(b)]
  if (one.size !== two.size) return false
  return (yield* digest(a)) === (yield* digest(b))
})

/** Whether the process a claim names is alive. */
const alive = (pid: number): boolean => {
  try {
    process.kill(pid, 0)
    return true
  } catch (error) {
    return (error as NodeJS.ErrnoException).code === "EPERM"
  }
}

/**
 * The claim on copy name `stem`, held until the scope closes: created
 * exclusively; a claim whose holder is gone is stale and taken over; one a
 * live take holds is waited for.
 */
const claim = Effect.fn("InboxTake.claim")(function*(pcaps: string, stem: string) {
  const fs = yield* FileSystem.FileSystem
  const file = claimFile(pcaps, stem)
  for (;;) {
    const made = yield* Effect.try({
      try: () => {
        nodeFs.writeFileSync(file, `${process.pid}\n`, { flag: "wx" })
        return true
      },
      catch: (error) => error as NodeJS.ErrnoException
    }).pipe(Effect.catch((error) => (error.code === "EEXIST" ? Effect.succeed(false) : Effect.die(error))))
    if (made) {
      yield* Effect.addFinalizer(() => fs.remove(file, { force: true }).pipe(Effect.ignore))
      return
    }
    const holder = Number((yield* fs.readFileString(file).pipe(Effect.orElseSucceed(() => ""))).trim())
    if (!Number.isInteger(holder) || holder <= 0 || !alive(holder)) {
      yield* fs.remove(file, { force: true })
      continue
    }
    yield* Effect.sleep(CLAIM_POLL_MS)
  }
})

/** Why a file of the take is not one the inbox holds directly, or undefined. */
const outside = (files: ReadonlyArray<File>, inbox: string): string | undefined => {
  const astray = files.find((file) => path.dirname(path.resolve(file.path)) !== path.resolve(inbox))
  return astray === undefined ? undefined : `${astray.path} is not directly under the inbox ${inbox}: a take removes what it takes`
}

/**
 * The raw files that still stand as the take found them removed from the
 * inbox, the others kept; the sidecar goes once every file has.
 */
const leaveInbox = Effect.fn("InboxTake.leaveInbox")(function*(files: ReadonlyArray<File>) {
  const fs = yield* FileSystem.FileSystem
  const removed: Array<string> = []
  const kept: Array<string> = []
  for (const file of files) {
    if (yield* unchanged(file)) {
      yield* fs.remove(file.path)
      removed.push(file.path)
    } else {
      kept.push(file.path)
    }
  }
  if (kept.length === 0) yield* fs.remove(sidecarOf(files[0]!.path), { force: true })
  return { removed, kept }
})

/** The file `name` of a capture of stem `stem` lands as under the `n`-th name: its stem replaced. */
const asIsName = (name: string, stem: string, n: number): string => nth(stem, n) + name.slice(stem.length)

/** What a take did to the inbox's raw files. */
interface Left {
  readonly removed: ReadonlyArray<string>
  readonly kept: ReadonlyArray<string>
}

/**
 * A take's COMMIT, uninterruptible: the marker written beside `target`, then
 * the capture `landing` renamed in as `target` (absent where `target` holds
 * it already), then `leave`, the raw files out of the inbox. A capture never
 * stands in `pcaps/` without its marker, and a cancelled take either landed
 * nothing or landed it whole; a crash leaves at most a marker with no
 * capture, which no listing reads and the next take of that name rewrites.
 */
const commit = <E, R>(
  target: string,
  marker: InboxMarker.Marker,
  landing: string | undefined,
  leave: Effect.Effect<Left, E, R>
) =>
  Effect.gen(function*() {
    yield* InboxMarker.write(target, marker)
    if (landing !== undefined) yield* move(landing, target)
    return yield* leave
  }).pipe(Effect.uninterruptible)

/**
 * A `none` take: the one file moved into `pcaps/` unchanged under its own
 * name, or the first `<stem>-n` name no other capture holds; where a name
 * holds these very bytes, the capture was landed already. A ring is refused:
 * the pcap root lists and replays a capture by its one file.
 */
const landAsIs = Effect.fn("InboxTake.landAsIs")(function*(request: Request, scope: Scope.Scope) {
  const fs = yield* FileSystem.FileSystem
  const { pcaps } = request.places
  if (request.files.length > 1) {
    return yield* new Refused({
      reason: `a ring of ${request.files.length} files lands at level none only as one file: merge it ` +
        `(mergecap -w <name>.pcap <its files>) and drop that, or take it at an anonymised level; its files stay in the inbox`
    })
  }
  const file = request.files[0]!
  const name = path.basename(file.path)
  const stem = copyStem(name)
  yield* fs.makeDirectory(pcaps, { recursive: true })
  const changed = new Refused({ reason: `${file.path} changed since the take was asked for it: it stays in the inbox` })
  if (!(yield* unchanged(file))) return yield* changed
  const sha256 = yield* digest(file.path)
  if (!(yield* unchanged(file))) return yield* changed
  const marker: InboxMarker.Marker = { level: "none", sha256 }
  for (let n = 1;; n++) {
    yield* claim(pcaps, nth(stem, n)).pipe(Scope.provide(scope))
    const target = path.join(pcaps, asIsName(name, stem, n))
    const held = yield* fs.exists(target)
    if (held && (yield* digest(target)) !== sha256) continue
    const moved = fs.remove(sidecarOf(file.path), { force: true }).pipe(Effect.as({ removed: [file.path], kept: [] }))
    const left = yield* commit(target, marker, held ? undefined : file.path, held ? leaveInbox([file]) : moved)
    return { pcap: target, level: request.level, seed: undefined, already: held, ...left } satisfies Taken
  }
})

/** The take's body, its claims and its work directory in `scope`. */
const takeIn = Effect.fn("InboxTake.takeIn")(function*(request: Request, scope: Scope.Scope) {
  if (request.level === "none") return yield* landAsIs(request, scope)
  const fs = yield* FileSystem.FileSystem
  const first = request.files[0]!
  const stem = copyStem(path.basename(first.path))
  const { pcaps, seeds } = request.places
  const anonymiser = request.anonymiser
  const raw = path.join(request.work, "raw")
  yield* fs.makeDirectory(raw, { recursive: true, mode: 0o700 })
  yield* fs.chmod(request.work, 0o700)
  yield* fs.chmod(raw, 0o700)
  yield* fs.makeDirectory(pcaps, { recursive: true })
  yield* fs.makeDirectory(seeds, { recursive: true, mode: 0o700 })
  const inputs = request.files.length === 1 ? [first.path] : request.files.flatMap((file) => ["--set", file.path])

  /** One anonymisation under `seed` into an empty directory of its own; the copy it wrote. */
  let runs = 0
  const anonymise = Effect.fn("InboxTake.anonymise")(function*(seed: string) {
    const out = path.join(request.work, `out${++runs}`)
    yield* fs.makeDirectory(out, { recursive: true })
    const exit = yield* run(anonymiser, [...inputs, out, "--seed", seed, "--raw-dir", raw, "--level", request.level])
    if (exit !== 0) {
      return yield* new Refused({
        reason: `the anonymisation did not verify (${path.basename(anonymiser)} exited ${exit}): the raw capture stays in the inbox`
      })
    }
    for (const name of [copyFile(stem), copyDocument(stem)]) {
      if (!(yield* fs.exists(path.join(out, name)))) {
        return yield* new Refused({ reason: `${path.basename(anonymiser)} wrote no ${name}: the raw capture stays in the inbox` })
      }
    }
    return path.join(out, copyFile(stem))
  })

  /** Where the copy lands: the first name free of copy and seed, claimed, under a fresh seed. */
  const landed = Effect.fn("InboxTake.landed")(function*() {
    yield* claim(pcaps, stem).pipe(Scope.provide(scope))
    const target = path.join(pcaps, copyFile(stem))
    const seed = path.join(seeds, seedFile(stem))
    const held = yield* fs.exists(target)
    if (!held || (yield* fs.exists(seed))) {
      const written = yield* anonymise(seed)
      if (!held) return { written, target, seed, already: false }
      if (yield* sameBytes(written, target)) return { written, target, seed, already: true }
    }
    for (let n = 2;; n++) {
      const other = nth(stem, n)
      yield* claim(pcaps, other).pipe(Scope.provide(scope))
      const otherTarget = path.join(pcaps, copyFile(other))
      const otherSeed = path.join(seeds, seedFile(other))
      if ((yield* fs.exists(otherTarget)) || (yield* fs.exists(otherSeed))) continue
      return { written: yield* anonymise(otherSeed), target: otherTarget, seed: otherSeed, already: false }
    }
  })

  const copy = yield* landed()
  yield* fs.chmod(copy.seed, 0o600)
  const sha256 = yield* digest(copy.written)
  const document = InboxMarker.documentOf(copy.target)
  yield* move(path.join(path.dirname(copy.written), copyDocument(stem)), document)
  const { removed, kept } = yield* commit(
    copy.target,
    { level: request.level, sha256, document, seed: copy.seed },
    copy.already ? undefined : copy.written,
    leaveInbox(request.files)
  )
  return { pcap: copy.target, level: request.level, seed: copy.seed, already: copy.already, removed, kept } satisfies Taken
})

export const take = Effect.fn("InboxTake.take")(function*(request: Request) {
  const fs = yield* FileSystem.FileSystem
  if (request.files.length === 0) return yield* new Refused({ reason: "an inbox take names the files of its capture" })
  const astray = outside(request.files, request.places.inbox)
  if (astray !== undefined) return yield* new Refused({ reason: astray })
  return yield* Effect.scopedWith((scope) => takeIn(request, scope)).pipe(
    Effect.ensuring(fs.remove(request.work, { recursive: true, force: true }).pipe(Effect.ignore))
  )
})
