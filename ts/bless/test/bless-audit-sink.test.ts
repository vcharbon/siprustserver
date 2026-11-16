/**
 * The bless audit's file sink: one NDJSON log per sweep, appended to by every
 * cell and read back whole, over the `FileSystem` service.
 */
import * as Cause from "effect/Cause"
import * as Effect from "effect/Effect"
import * as Exit from "effect/Exit"
import * as FileSystem from "effect/FileSystem"
import * as Layer from "effect/Layer"
import * as PlatformError from "effect/PlatformError"
import { describe, expect, it } from "vitest"
import * as BlessAudit from "../src/audit.js"

const LOG = "/sweeps/_audit/bless-7.ndjson"

const finding = (caseId: string): BlessAudit.AuditRecord => ({
  finding: "failure-shape-on-a-green-cell",
  lane: "lab",
  rules: ["final-class-kept"],
  signatures: ["shape:status-substitution:503->500:initial-invite"],
  caseId,
  detail: ""
})

const denied = (method: string, path: string): PlatformError.PlatformError =>
  PlatformError.systemError({ _tag: "PermissionDenied", module: "FileSystem", method, pathOrDescriptor: path })

/** An in-memory file system: directories made, strings appended, files read. */
const memory = (seed: Record<string, string> = {}, readOnly = false) => {
  const files = new Map<string, string>(Object.entries(seed))
  const dirs = new Set<string>()
  const layer = FileSystem.layerNoop({
    exists: (path) => Effect.succeed(files.has(path)),
    makeDirectory: (path) =>
      readOnly ? Effect.fail(denied("makeDirectory", path)) : Effect.sync(() => void dirs.add(path)),
    writeFileString: (path, data, options) =>
      readOnly
        ? Effect.fail(denied("writeFileString", path))
        : Effect.sync(() => void files.set(path, (options?.flag === "a" ? files.get(path) ?? "" : "") + data)),
    readFileString: (path) => {
      const text = files.get(path)
      return text === undefined
        ? Effect.fail(PlatformError.systemError({ _tag: "NotFound", module: "FileSystem", method: "readFileString", pathOrDescriptor: path }))
        : Effect.succeed(text)
    }
  })
  return { layer, files, dirs }
}

const through = <A, E>(fs: Layer.Layer<FileSystem.FileSystem>, use: (sink: BlessAudit.Interface) => Effect.Effect<A, E>) =>
  Effect.runSyncExit(
    Effect.flatMap(BlessAudit.Sink, use).pipe(Effect.provide(Layer.provide(BlessAudit.Sink.file(LOG), fs)))
  )

describe("the bless audit's file sink", () => {
  it("appends what every cell files and reads the sweep back whole, in filing order", () => {
    const disk = memory()
    const got = through(disk.layer, (sink) =>
      Effect.gen(function* () {
        yield* sink.append([finding("a")])
        yield* sink.append([])
        yield* sink.append([finding("b"), finding("c")])
        return yield* sink.read()
      })
    )
    expect(got).toEqual(Exit.succeed([finding("a"), finding("b"), finding("c")]))
    expect(disk.dirs.has("/sweeps/_audit")).toBe(true)
    expect(disk.files.get(LOG)!.split("\n").filter((l) => l !== "")).toHaveLength(3)
  })

  it("reads an unwritten sweep as no findings, and writes nothing where there are none", () => {
    const disk = memory()
    expect(through(disk.layer, (sink) => Effect.andThen(sink.append([]), sink.read()))).toEqual(Exit.succeed([]))
    expect(disk.files.size).toBe(0)
  })

  it("names the line that is not a record", () => {
    const got = through(memory({ [LOG]: `${JSON.stringify(finding("a"))}\n{"finding":"nope"}\n` }).layer, (sink) => sink.read())
    expect(Exit.isFailure(got)).toBe(true)
    if (Exit.isFailure(got)) {
      const error = Cause.squash(got.cause)
      expect(error).toBeInstanceOf(BlessAudit.AuditLogCorrupt)
      expect((error as BlessAudit.AuditLogCorrupt).line).toBe(`{"finding":"nope"}`)
    }
  })

  it("names the log it could not append to", () => {
    const got = through(memory({}, true).layer, (sink) => sink.append([finding("a")]))
    expect(Exit.isFailure(got)).toBe(true)
    if (Exit.isFailure(got)) {
      const error = Cause.squash(got.cause)
      expect(error).toBeInstanceOf(BlessAudit.AuditLogUnreachable)
      expect((error as BlessAudit.AuditLogUnreachable).path).toBe(LOG)
    }
  })

  it("the silent sink files nothing and reads nothing", () => {
    const got = Effect.runSync(
      Effect.flatMap(BlessAudit.Sink, (sink) => Effect.andThen(sink.append([finding("a")]), sink.read())).pipe(
        Effect.provide(BlessAudit.Sink.silent)
      )
    )
    expect(got).toEqual([])
  })
})
