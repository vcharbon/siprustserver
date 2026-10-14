/**
 * What a `--case` list comes to before anything is planned: every cited form
 * read back to a case id of ONE capture, that capture named where the line
 * named none, and every id that names no call refused with the reason.
 */
import { Resolver, StoreBinding } from "@sip/store"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { describe, expect, it } from "vitest"
import * as CaseSelection from "../src/commands/case-selection.js"
import type { Selection } from "@sip/corpus/universe"

const C = "trace.pcap.gz"
const D = "other.pcap.gz"

const selection = (positional: ReadonlyArray<string>, cases: ReadonlyArray<string>): Selection => ({
  universe: "named",
  positional,
  onlyNew: false,
  maxSpanSeconds: undefined,
  greenOnLane: undefined,
  sampleSize: undefined,
  limit: undefined,
  cases
})

const call = (capture: string, caseId: string, over: Partial<Resolver.Call> = {}): Resolver.Call => ({
  capture,
  caseId,
  callKey: `${caseId}@key`,
  prepId: 7,
  kind: "case",
  reasons: [],
  standing: true,
  ...over
})

const answer = (
  input: string,
  status: Resolver.Status,
  calls: ReadonlyArray<Resolver.Call>,
  note: string
): Resolver.Resolution => ({ input, form: Resolver.formOf(input), status, calls, total: calls.length, note })

const ANSWERS: ReadonlyMap<string, Resolver.Resolution> = new Map([
  [`${C}-case110`, answer(`${C}-case110`, "resolved", [call(C, `${C}-case110`)], "prep7")],
  ["trace-case110", answer("trace-case110", "resolved", [call(C, `${C}-case110`)], "prep7")],
  ["run322903/case110", answer("run322903/case110", "resolved", [call(C, `${C}-case110`)], "run 322903 (…): prep7")],
  [`${D}-case1`, answer(`${D}-case1`, "resolved", [call(D, `${D}-case1`)], "prep7")],
  [
    "case1",
    answer("case1", "ambiguous", [call(C, `${C}-case1`), call(D, `${D}-case1`)], "2 calls are named -case1: name the capture")
  ],
  [
    `${C}-case9`,
    answer(
      `${C}-case9`,
      "refused",
      [call(C, `${C}-case9`, { kind: "refused", reasons: ["source-call-incomplete"] })],
      "refused by prep7: source-call-incomplete"
    )
  ],
  ["nothing", answer("nothing", "not-found", [], `nothing the store holds is named "nothing": one of ${Resolver.FORMS}`)]
])

/** Every input the table states; any other is a lookup the test did not expect. */
const resolver = Layer.succeed(
  Resolver.Service,
  Resolver.Service.of({
    resolve: (input) => {
      const found = ANSWERS.get(input)
      return found === undefined ? Effect.die(`unexpected lookup of ${input}`) : Effect.succeed(found)
    }
  })
)

/** The reason a refusal token is printed with. */
const why = (token: string): string => `because ${token}`

const read = (positional: ReadonlyArray<string>, cases: ReadonlyArray<string>) =>
  Effect.runPromise(CaseSelection.resolve(selection(positional, cases), why).pipe(Effect.provide(Layer.merge(resolver, StoreBinding.layer))))

describe("CaseSelection.resolve", () => {
  it("leaves a selection naming no case as it was", async () => {
    const before = selection([C], [])
    expect(await Effect.runPromise(CaseSelection.resolve(before, why).pipe(Effect.provide(Layer.merge(resolver, StoreBinding.layer))))).toEqual({
      _tag: "read",
      selection: before
    })
  })

  it("reads a bare suffix as the named capture's case, with no lookup", async () => {
    const got = await read([C], ["case110", "-case111"])
    expect(got).toEqual({ _tag: "read", selection: selection([C], [`${C}-case110`, `${C}-case111`]) })
  })

  it("names the capture a full case id names where the line named none", async () => {
    expect(await read([], [`${C}-case110`])).toEqual({ _tag: "read", selection: selection([C], [`${C}-case110`]) })
  })

  it("reads a case id cited by its capture's stem, and runN/caseN, back to the capture's own id", async () => {
    expect(await read([], ["trace-case110", "run322903/case110"])).toEqual({
      _tag: "read",
      selection: selection([C], [`${C}-case110`])
    })
  })

  it("refuses a refused call with its reasons", async () => {
    const got = await read([], [`${C}-case9`])
    expect(got._tag).toBe("refused")
    expect(got._tag === "refused" ? got.why : "").toContain("source-call-incomplete: because source-call-incomplete")
  })

  it("refuses an ambiguous bare suffix naming its candidates and asking for the capture", async () => {
    const got = await read([], ["case1"])
    expect(got._tag).toBe("refused")
    const why = got._tag === "refused" ? got.why : ""
    expect(why).toContain(`${C}-case1`)
    expect(why).toContain(`${D}-case1`)
    expect(why).toContain("name the capture")
  })

  it("refuses an id nothing holds naming the accepted forms", async () => {
    const got = await read([], ["nothing"])
    expect(got._tag === "refused" ? got.why : "").toContain(Resolver.FORMS)
  })

  it("refuses ids of two captures, naming both", async () => {
    const got = await read([], [`${C}-case110`, `${D}-case1`])
    const why = got._tag === "refused" ? got.why : ""
    expect(why).toContain(C)
    expect(why).toContain(D)
  })

  it("refuses an id of another capture than the one named", async () => {
    const got = await read([C], [`${D}-case1`])
    const why = got._tag === "refused" ? got.why : ""
    expect(why).toContain(`${D}-case1`)
    expect(why).toContain(C)
  })
})
