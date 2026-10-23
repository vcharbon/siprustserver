/**
 * A node child runs inside its memory scope with its V8 heap sized to that
 * scope, unless the caller states a heap of its own, which wins.
 */
import { describe, expect, it } from "vitest"
import { environment, heapMbOf, withHeap } from "../src/node-heap.js"

describe("the heap a scope allows", () => {
  it("is four fifths of a size systemd takes, in MiB", () => {
    expect(heapMbOf("4G")).toBe(3276)
    expect(heapMbOf("12G")).toBe(9830)
    expect(heapMbOf("512M")).toBe(409)
    expect(heapMbOf("1.5G")).toBe(1228)
    expect(heapMbOf("1G512M")).toBe(1228)
    expect(heapMbOf(String(1024 ** 3))).toBe(819)
  })

  it("is none where systemd would refuse the size or it states no absolute one", () => {
    expect(heapMbOf("4g")).toBeUndefined()
    expect(heapMbOf("512m")).toBeUndefined()
    expect(heapMbOf("infinity")).toBeUndefined()
    expect(heapMbOf("50%")).toBeUndefined()
    expect(heapMbOf("0")).toBeUndefined()
    expect(heapMbOf("")).toBeUndefined()
  })
})

describe("the node options a child is given", () => {
  it("keep a heap size the caller set, whatever the scope allows", () => {
    expect(withHeap("--enable-source-maps --max-old-space-size=7000", 3276)).toEqual({
      options: "--enable-source-maps --max-old-space-size=7000",
      note: "the caller's --max-old-space-size=7000 wins over the 3276 MiB the memory scope allows"
    })
    expect(withHeap("--max-old-space-size=7000", undefined)).toEqual({ options: "--max-old-space-size=7000" })
  })

  it("carry the scope's heap where the caller set none", () => {
    expect(withHeap(undefined, 3276)).toEqual({ options: "--max-old-space-size=3276" })
    expect(withHeap("--cpu-prof", 3276)).toEqual({ options: "--cpu-prof --max-old-space-size=3276" })
    expect(withHeap("--cpu-prof", undefined)).toEqual({ options: "--cpu-prof" })
    expect(withHeap(undefined, undefined)).toEqual({ options: undefined })
  })
})

describe("the environment of a node child", () => {
  it("carries the scope's heap in NODE_OPTIONS", () => {
    expect(environment("8G", { PATH: "/bin" })).toEqual({
      env: { PATH: "/bin", NODE_OPTIONS: "--max-old-space-size=6553" }
    })
  })

  it("keeps the caller's heap, and says so where the scope states one too", () => {
    const heap = environment("8G", { PATH: "/bin", NODE_OPTIONS: "--max-old-space-size=7000" })
    expect(heap.env).toEqual({ PATH: "/bin", NODE_OPTIONS: "--max-old-space-size=7000" })
    expect(heap.note).toContain("wins")
  })

  it("leaves the caller's environment as it is where the scope states no size", () => {
    expect(environment("infinity", { PATH: "/bin" }).env).toEqual({ PATH: "/bin" })
    expect(environment("infinity", { PATH: "/bin", NODE_OPTIONS: "--max-old-space-size=7000" }).env)
      .toEqual({ PATH: "/bin", NODE_OPTIONS: "--max-old-space-size=7000" })
  })
})
