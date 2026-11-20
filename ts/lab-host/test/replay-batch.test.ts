/**
 * What an end line states the backend served, under its `bl` key: its
 * requests and process time, none where the line states none or does not
 * read; the words it prints in are the pack's.
 */
import { describe, expect, it } from "vitest"
import { servedOf } from "../src/replay-batch.js"

describe("what the backend served", () => {
  it("is the end line's count and process time, or its count alone", () => {
    expect(servedOf(`{"end":{"dirty":null,"bl":{"served":7,"process_us":{"p50":3,"p99":9,"max":12}}}}`))
      .toEqual({ served: 7, processUs: { p50: 3, p99: 9, max: 12 } })
    expect(servedOf(`{"end":{"dirty":null,"bl":{"served":0,"process_us":null}}}`)).toEqual({ served: 0, processUs: null })
  })

  it("is none of an end line stating no backend, a line that is no end line, or one that is no JSON", () => {
    expect(servedOf(`{"end":{"dirty":null}}`)).toBeUndefined()
    expect(servedOf(`{"ready":{}}`)).toBeUndefined()
    expect(servedOf("not json")).toBeUndefined()
  })
})
