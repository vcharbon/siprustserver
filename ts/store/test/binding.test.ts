/**
 * The store binding: a selector parsed through it reads in the deployment's
 * spelling, and the default binding is the store's own spelling and grammar.
 */
import * as Effect from "effect/Effect"
import { describe, expect, it } from "vitest"
import * as StoreBinding from "../src/binding.js"
import * as Migrator from "../src/migrator.js"
import * as Resolver from "../src/resolver.js"
import * as Selector from "../src/selector.js"

const terms = (parsed: Selector.Selector | { readonly refused: string }) =>
  Selector.isRefused(parsed) ? parsed.refused : parsed.terms

describe("StoreBinding", () => {
  it("parses in the deployment's spelling of the current term", async () => {
    const binding = StoreBinding.layerWith({
      spelling: { current: "@main" },
      grammar: Resolver.DEFAULT_GRAMMAR,
      migrate: Migrator.run
    })
    const read = await Effect.runPromise(
      Effect.all([StoreBinding.parse("@main/@lane:fake"), StoreBinding.parse("@current"), StoreBinding.current]).pipe(
        Effect.provide(binding)
      )
    )
    expect(terms(read[0])).toEqual([{ _tag: "current" }, { _tag: "lane", lane: "fake" }])
    expect(terms(read[1])).toContain("one of @main,")
    expect(read[2]).toBe("@main")
  })

  it("defaults to the store's own spelling and grammar", async () => {
    const binding = await Effect.runPromise(StoreBinding.Service.pipe(Effect.provide(StoreBinding.layer)))
    expect(binding.spelling).toEqual(Selector.DEFAULT_SPELLING)
    expect(binding.grammar).toBe(Resolver.DEFAULT_GRAMMAR)
  })
})
