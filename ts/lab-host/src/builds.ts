/**
 * The binaries a tooling shells out to, ready to exec: where each is (its
 * pack's `BinaryRule`, one rule for all of them) and, in a checkout, brought
 * up to date first.
 *
 * An install of prebuilt binaries (the rule's variable) never runs cargo: a
 * binary is taken from the directory as it is, and one the directory lacks is
 * an error naming it.
 *
 * A checkout always builds, never only-if-absent: a run records the working
 * tree's provenance, so replaying yesterday's binary under today's sha would
 * file the answer under the wrong code. Built once per process: a replay builds
 * before it takes its locks (`@sip/corpus` `replay/build.ts`), and a later call
 * in the same process finds the binary current instead of running cargo inside
 * the window.
 *
 * ONE build at a time box-wide (`@sip/corpus` `Locks.exclusive("cargo")`),
 * inside the box's cargo scope where a user session answers
 * (`user-session.ts`): two cargo builds on one host take the host down,
 * whichever checkouts they run in. A second caller WAITS rather than being
 * refused — waiting costs it a no-op cargo run, a refusal would cost it its
 * sweep. Each build prints a `[timing]` line, `build.<what>`, its wait for the
 * lock included.
 */
import type { Roots } from "@sip/corpus"
import * as Locks from "@sip/corpus/locks"
import * as Timing from "@sip/corpus/timing"
import * as BinaryRule from "@sip/toolchain/binary-rule"
import * as HostTools from "@sip/toolchain/host-tools"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import { spawnSync } from "node:child_process"
import * as path from "node:path"
import type { LabPack } from "./lab-pack.js"
import * as UserSession from "./user-session.js"

/** The facts a build reads off its pack. */
export type BuildPack = Pick<LabPack, "binaries" | "variables" | "buildScope" | "buildScoped">

/** The build's scope properties, as the pack caps it: its `MemoryMax`, then the pack's other caps. */
export const scopeOf = (pack: Pick<LabPack, "buildScope">): ReadonlyArray<string> => [
  `MemoryMax=${pack.buildScope.memoryMax}`,
  ...pack.buildScope.properties
]

/**
 * The build's environment. Inside {@link scopeOf} (`session`): `env` with
 * what the pack tells its build wrapper of a capped scope (`buildScoped`), so
 * the wrapper opens no scope of its own that would move the build out of the
 * scope's caps. With no session, `env` as it is.
 */
export const buildEnv = (pack: BuildPack, env: NodeJS.ProcessEnv, session: boolean): NodeJS.ProcessEnv =>
  session ? { ...env, ...pack.buildScoped(pack.buildScope.memoryMax) } : { ...env }

/** Long enough for a cold release build of both trees to finish ahead of us. */
const BUILD_WAIT_MS = 1_800_000

/** What a build reaches for: the lock root and the files the lock lives in. */
export type Requirements = Roots.Service | FileSystem.FileSystem

/**
 * One cargo build with {@link buildEnv}, inside {@link scopeOf} where a user
 * session answers (`session`), plain where none does; throws when it fails.
 */
export const cargo = (
  pack: BuildPack,
  what: string,
  cwd: string,
  args: ReadonlyArray<string>,
  session: boolean
): void => {
  console.log(`[build] ${what} (release)…`)
  const line = UserSession.scoped(scopeOf(pack), "cargo", args, session)
  const r = spawnSync(line.command, [...line.args], {
    cwd,
    stdio: "inherit",
    env: buildEnv(pack, process.env, session)
  })
  if (r.status !== 0) throw new Error(`${what} build failed`)
}

/** The binaries this process has brought up to date. */
const current = new Set<string>()

/** How long this process's cargo runs took, their waits for the build lock left out. */
let compiling = 0

/** The milliseconds this process has spent in cargo so far, waits for another build's lock excluded. */
export const compileMs = (): number => compiling

/** `run`, its time added to {@link compileMs}: what runs here is compiling, never waiting for a lock. */
export const timeCompile = (run: () => void): void => {
  const clock = Timing.stopwatch()
  try {
    run()
  } finally {
    compiling += clock.total()
  }
}

/** Who hears whose build is waited for: a job writes it on its row. */
export type Watch = Locks.Watch

/** One binary a tooling execs, and the cargo build that makes it in a checkout. */
export interface Binary {
  readonly name: string
  readonly what: string
  /** Where cargo runs. */
  readonly cwd: string
  readonly build: ReadonlyArray<string>
}

const exists = (file: string) =>
  Effect.flatMap(FileSystem.FileSystem, (fs) => fs.exists(file)).pipe(Effect.orElseSucceed(() => false))

/** `binary` taken as it is from the prebuilt directory `dir`; one it lacks, or cannot exec, is an error. */
const prebuilt = (pack: BuildPack, binary: Binary, dir: string): Effect.Effect<string, Error> =>
  Effect.gen(function*() {
    const file = path.join(dir, binary.name)
    if (yield* HostTools.isExecutable(file)) return file
    return yield* Effect.fail(
      new Error(`${binary.what}: no executable ${binary.name} in ${pack.binaries.variable} ${dir}`)
    )
  })

/**
 * `binary` brought up to date by one build under the box's build lock, timed;
 * one this process already built is current. `watch` hears whose build is
 * waited for.
 */
const built = (pack: BuildPack, binary: Binary, watch?: Watch): Effect.Effect<string, Error, Requirements> =>
  Effect.gen(function*() {
    const file = BinaryRule.releaseBuild(pack.binaries, binary.name)
    if (current.has(file)) return file
    const clock = Timing.stopwatch()
    const lock = yield* Locks.exclusive("cargo", `building ${binary.what}`, { waitMs: BUILD_WAIT_MS }, watch).pipe(
      Effect.mapError((error) => new Error(error.message))
    )
    if (!Locks.isTaken(lock)) return yield* Effect.fail(new Error(`${binary.what} build refused: ${lock.held}`))
    const session = yield* UserSession.answers(pack.variables.userSession)
    yield* Effect.try({
      try: () => timeCompile(() => cargo(pack, binary.what, binary.cwd, binary.build, session)),
      catch: (cause) => (cause instanceof Error ? cause : new Error(String(cause)))
    }).pipe(Effect.ensuring(lock.release))
    if (!(yield* exists(file))) return yield* Effect.fail(new Error(`${binary.what} build failed: no ${file}`))
    current.add(file)
    Timing.emit({ phase: `build.${binary.what}`, ms: clock.total() })
    return file
  })

/** `binary`, ready to exec: from the prebuilt directory where this process names one, else built. */
export const ready = (pack: BuildPack, binary: Binary, watch?: Watch): Effect.Effect<string, Error, Requirements> =>
  Effect.suspend(() => {
    const dir = BinaryRule.binDir(pack.binaries)
    return dir === undefined ? built(pack, binary, watch) : prebuilt(pack, binary, dir)
  })

/** The flows tool of `pack`'s generic SIP tree: what a cut enriches with. */
export const sipflow = (pack: Pick<LabPack, "sipTree">): Binary => ({
  name: "sipflow",
  what: "sipflow",
  cwd: pack.sipTree,
  build: ["build", "--release", "-p", "sip-pcap", "--bin", "sipflow"]
})

/** The pivot schema tool of `pack`'s generic SIP tree: what a document is linted with. */
export const pivotSchema = (pack: Pick<LabPack, "sipTree">): Binary => ({
  name: "pivot-schema",
  what: "pivot-schema",
  cwd: pack.sipTree,
  build: ["build", "--release", "-p", "pivot-schema", "--bin", "pivot-schema"]
})

/** The builds over `pack`'s facts. */
export const make = (pack: BuildPack) => ({
  buildEnv: (env: NodeJS.ProcessEnv, session: boolean) => buildEnv(pack, env, session),
  cargo: (what: string, cwd: string, args: ReadonlyArray<string>, session: boolean) =>
    cargo(pack, what, cwd, args, session),
  ready: (binary: Binary, watch?: Watch) => ready(pack, binary, watch)
})
