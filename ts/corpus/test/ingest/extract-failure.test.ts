/**
 * An extraction failure's line: its step first, then the binary and how it
 * ended, then what it said. The real child endings (exited, killed, could not
 * start) are run in `generation.test.ts`; here, the endings a test cannot
 * provoke cheaply and the failures that are no child's.
 */
import { describe, expect, it } from "vitest"
import * as ExtractFailure from "../../src/ingest/extract-failure.js"

describe("ExtractFailure.lineOf", () => {
  it("says a child ran but its output was not written, with what it said on one line", () => {
    const line = ExtractFailure.lineOf({
      _tag: "Child",
      step: "read",
      binary: "/opt/bin/sipflow",
      ending: { _tag: "Unwritten", why: "writing /store/raw failed: ENOSPC" },
      stderr: "warn: \"late\"\npacket\n"
    })
    expect(line).toBe("read: sipflow ran, but its output was not written: writing /store/raw failed: ENOSPC; stderr: warn: late packet")
  })

  it("leaves no stack frame of a child's stderr in the line", () => {
    const line = ExtractFailure.lineOf({
      _tag: "Child",
      step: "index",
      binary: "sipflow",
      ending: { _tag: "Exited", code: 70 },
      stderr: "Error: broken\n    at run (/srv/index.js:10:5)\n"
    })
    expect(line).toBe("index: sipflow exited 70; stderr: Error: broken")
  })

  it("prefixes the anonymiser's own word and an internal death with their steps", () => {
    expect(ExtractFailure.lineOf({ _tag: "Anonymise", line: "a number survived" })).toBe("anonymise: a number survived")
    expect(ExtractFailure.lineOf({ _tag: "Internal", message: "EACCES: /store/x" }))
      .toBe("internal: the extraction died: EACCES: /store/x")
  })
})
