/**
 * A copy goes through the Clipboard API where it exists and accepts the
 * write, through the `copy` command otherwise, and says false where neither
 * copied.
 */
import { describe, expect, it } from "vitest"
import { copyWith } from "../src/clipboard.ts"

/** The paths a copy took, and what each answers. */
const paths = (api: "absent" | "ok" | "refused", command: boolean | "throws") => {
  const took: string[] = []
  return {
    took,
    paths: {
      writeText: api === "absent" ? undefined : (text: string) => {
        took.push(`api ${text}`)
        return api === "ok" ? Promise.resolve() : Promise.reject(new Error("NotAllowedError"))
      },
      copyCommand: (text: string) => {
        took.push(`command ${text}`)
        if (command === "throws") throw new Error("unsupported")
        return command
      }
    }
  }
}

describe("copying text", () => {
  it("writes through the Clipboard API where it accepts the write", async () => {
    const at = paths("ok", true)
    expect(await copyWith(at.paths, "x")).toBe(true)
    expect(at.took).toEqual(["api x"])
  })

  it("falls back to the copy command where the API is absent (an insecure origin) or refuses", async () => {
    const absent = paths("absent", true)
    expect(await copyWith(absent.paths, "x")).toBe(true)
    expect(absent.took).toEqual(["command x"])
    const refused = paths("refused", true)
    expect(await copyWith(refused.paths, "x")).toBe(true)
    expect(refused.took).toEqual(["api x", "command x"])
  })

  it("says false where neither copied", async () => {
    expect(await copyWith(paths("refused", false).paths, "x")).toBe(false)
    expect(await copyWith(paths("absent", "throws").paths, "x")).toBe(false)
  })
})
