/**
 * A failed GET is the server's stated reason: the `detail` of a JSON body,
 * whatever the status, and the status line with the body only where the
 * server states none.
 */
import { afterEach, describe, expect, it, vi } from "vitest"
import { api } from "../src/api.ts"

const answer = (status: number, body: string, statusText: string): void => {
  vi.stubGlobal("fetch", () => Promise.resolve(new Response(body, { status, statusText })))
}

afterEach(() => {
  vi.unstubAllGlobals()
})

describe("a failed GET", () => {
  it("carries the detail an answer that did not encode states", async () => {
    answer(500, JSON.stringify({ detail: "cell: the answer did not encode at [\"http\"]" }), "Internal Server Error")
    await expect(api.triageCell("capture", "run1", "cell")).rejects.toThrow(
      /^cell: the answer did not encode at \["http"\]$/
    )
  })

  it("falls back to the status line and the body where the server states no detail", async () => {
    answer(502, "upstream gone", "Bad Gateway")
    await expect(api.triageCell("capture", "run1", "cell")).rejects.toThrow("502 Bad Gateway — upstream gone")
  })
})
