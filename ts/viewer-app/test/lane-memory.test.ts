/**
 * Which lane a page opens on: the URL's lane when it carries one, else the
 * lane the browser remembers from the last picker. The memory is a
 * `Storage`-like store under one key; a store that throws (a private window)
 * reads as nothing remembered and takes no write.
 */
import { describe, expect, it } from "vitest"
import { LANE_KEY, recall, remember, resolve, type LaneStore } from "../src/lane-memory.ts"

/** A store that keeps what it is given. */
const memory = (): LaneStore & { readonly items: Map<string, string> } => {
  const items = new Map<string, string>()
  return {
    items,
    getItem: (key) => items.get(key) ?? null,
    setItem: (key, value) => {
      items.set(key, value)
    },
    removeItem: (key) => {
      items.delete(key)
    }
  }
}

/** A store every call of which throws, as `window.localStorage` does in a private window. */
const throwing: LaneStore = {
  getItem: () => {
    throw new Error("SecurityError: storage is disabled")
  },
  setItem: () => {
    throw new Error("SecurityError: storage is disabled")
  },
  removeItem: () => {
    throw new Error("SecurityError: storage is disabled")
  }
}

describe("resolve", () => {
  it("takes the URL's lane over the remembered one", () => {
    expect(resolve("real", "fake")).toBe("real")
  })

  it("reads the remembered lane when the URL carries none", () => {
    expect(resolve(null, "fake")).toBe("fake")
  })

  it("is the configured lane when neither the URL nor the memory names one", () => {
    expect(resolve(null, undefined)).toBe("")
  })
})

describe("remember and recall", () => {
  it("writes the lane under the one key and reads it back", () => {
    const store = memory()
    remember(store, "fake")
    expect(store.items.get(LANE_KEY)).toBe("fake")
    expect(recall(store)).toBe("fake")
  })

  it("clears the memory when the configured lane (empty) is chosen", () => {
    const store = memory()
    remember(store, "fake")
    remember(store, "")
    expect(store.items.has(LANE_KEY)).toBe(false)
    expect(recall(store)).toBeUndefined()
  })

  it("recalls nothing from a store that was never written", () => {
    expect(recall(memory())).toBeUndefined()
  })

  it("reads a throwing store as nothing remembered, and writes it nothing", () => {
    expect(recall(throwing)).toBeUndefined()
    expect(() => remember(throwing, "fake")).not.toThrow()
    expect(() => remember(throwing, "")).not.toThrow()
  })
})
