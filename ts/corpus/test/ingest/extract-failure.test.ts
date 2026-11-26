/**
 * An extraction failure's line and kind: its step first, then the binary and
 * how it ended, then what it said; and whose the failure is. The real child
 * endings (exited, killed, could not be started) are run in
 * `generation.test.ts`; here, the endings a test cannot provoke cheaply and
 * the failures that are no child's.
 */
import { describe, expect, it } from "vitest"
import * as ExtractFailure from "../../src/ingest/extract-failure.js"

const child = (
  step: ExtractFailure.ChildStep,
  ending: ExtractFailure.Ending,
  stderr = ""
): ExtractFailure.Failure => ({ _tag: "Child", step, binary: "/opt/bin/sipflow", ending, stderr })

describe("ExtractFailure.lineOf", () => {
  it("says a child ran but its output was not written, with what it said on one line", () => {
    const line = ExtractFailure.lineOf(
      child("read", { _tag: "Unwritten", why: "writing /store/raw failed: ENOSPC" }, "warn: \"late\"\npacket\n")
    )
    expect(line).toBe("read: sipflow ran, but its output was not written: writing /store/raw failed: ENOSPC; stderr: warn: late packet")
  })

  it("says a child whose exit could not be read ran, apart from one that never started", () => {
    expect(ExtractFailure.lineOf(child("index", { _tag: "Unawaited", why: "EPIPE" })))
      .toBe("index: sipflow ran, and its exit could not be read: EPIPE; stderr: (no stderr)")
  })

  it("leaves no stack frame of a child's stderr in the line", () => {
    const line = ExtractFailure.lineOf(
      child("index", { _tag: "Exited", code: 70 }, "Error: broken\n    at run (/srv/index.js:10:5)\n")
    )
    expect(line).toBe("index: sipflow exited 70; stderr: Error: broken")
  })

  it("prefixes the anonymiser's own word and an internal death with their steps", () => {
    expect(ExtractFailure.lineOf({ _tag: "Anonymise", line: "a number survived" })).toBe("anonymise: a number survived")
    expect(ExtractFailure.lineOf({ _tag: "Internal", message: "EACCES: /store/x" }))
      .toBe("internal: the extraction died: EACCES: /store/x")
  })
})

describe("ExtractFailure.kindOf", () => {
  it("calls the capture's own only a read that exited on it and the anonymiser's refusal", () => {
    expect(ExtractFailure.kindOf(child("read", { _tag: "Exited", code: 1 }))).toBe("capture")
    expect(ExtractFailure.kindOf({ _tag: "Anonymise", line: "a number survived" })).toBe("capture")
    const runs: ReadonlyArray<ExtractFailure.Failure> = [
      child("read", { _tag: "Killed", signal: "SIGKILL", oom: null }),
      child("read", { _tag: "Unstarted", why: "ENOENT" }),
      child("read", { _tag: "Unawaited", why: "EPIPE" }),
      child("read", { _tag: "Unwritten", why: "ENOSPC" }),
      child("index", { _tag: "Exited", code: 1 }),
      { _tag: "Internal", message: "EACCES" }
    ]
    expect(runs.map(ExtractFailure.kindOf)).toEqual(runs.map(() => "run"))
  })
})
