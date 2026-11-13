/**
 * An operation as a step of a case (ADR-0060 appendix §7): an `inject` node of
 * the cell's document whose action is `operation:<name>`, injected at the
 * point its `after` names. The operations a cell asks for are its steps'; the
 * tags its case carries ride beside its document; a source yielding a cell
 * whose document injects anything else, or an operation step naming no point
 * of its call, is refused naming the cell and the node.
 */
import { describe, expect, it } from "vitest"
import * as Result from "effect/Result"
import * as CaseSource from "../src/case-source.js"
import * as OperationStep from "../src/operation-step.js"

const DELAY = { ms: 0, from: "trigger", compressible: true, timer_linked: false }

/** A cell whose caller sends an INVITE (`s1`) and receives its 200 (`s2`), `injects` placed after them. */
const cellInjecting = (id: string, injects: ReadonlyArray<Record<string, unknown>>, tags?: ReadonlyArray<string>) => ({
  id,
  case: "a case",
  title: "a case",
  document: {
    pivot_version: 3,
    case: { id, title: "a case", family: "transparent", variant: "target", origin: "authored", lanes: { "*": "ok" } },
    identities: [],
    calls: [],
    endpoints: [{ id: "e", observed: "192.0.2.10:5060", side: "peer", binding: "dedicated" }],
    actors: [{ id: "a", kind: "uac", endpoint: "e" }],
    legs: [{ id: "A", actor: "a", dir: "out" }],
    flow: [
      { id: "s1", leg: "A", op: "send", msg: { method: "INVITE" }, delay: DELAY },
      { id: "s2", leg: "A", op: "expect", check: "assert", msg: { status: 200, "cseq-method": "INVITE" }, delay: DELAY },
      ...injects
    ],
    timing: { expect_budget_ms: 1000, settle_budget_ms: 1000 }
  },
  resources: {},
  claims: { "call-records": { count: 1 } },
  ...(tags === undefined ? {} : { tags })
})

const loaded = (value: unknown) => CaseSource.cellsOf("cases.ts", value)

const refusalOf = (value: unknown): string => {
  const cells = loaded(value)
  return Result.isFailure(cells) ? cells.failure.message : "accepted"
}

describe("a cell's operation steps", () => {
  it("are its document's injects of operation:<name>, each at the point its after names; the operations it asks for are theirs, once each", () => {
    const cell = Result.getOrThrow(loaded([
      cellInjecting("stepped", [
        { id: "i1", op: "inject", action: "operation:restart-worker", after: ["s2"] },
        { id: "i2", op: "inject", action: "operation:block-route", after: ["s1"] },
        { id: "i3", op: "inject", action: "operation:restart-worker", after: ["s2", "i1"] }
      ])
    ]))[0]!
    expect(OperationStep.stepsOf(cell.document)).toEqual([
      { node: "i1", operation: "restart-worker", after: ["s2"] },
      { node: "i2", operation: "block-route", after: ["s1"] },
      { node: "i3", operation: "restart-worker", after: ["s2", "i1"] }
    ])
    expect(OperationStep.operationsOf(cell.document)).toEqual(["restart-worker", "block-route"])
  })

  it("are none where its document injects nothing", () => {
    const cell = Result.getOrThrow(loaded([cellInjecting("plain", [])]))[0]!
    expect(OperationStep.operationsOf(cell.document)).toEqual([])
  })

  it("name the action a lane binding the operation performs: its capability", () => {
    expect(OperationStep.actionOf("restart-worker")).toBe("operation:restart-worker")
  })
})

describe("a cell's tags", () => {
  it("are those its case carries, beside its document", () => {
    const cell = Result.getOrThrow(loaded([cellInjecting("tagged", [], ["disruptive"])]))[0]!
    expect(cell.tags).toEqual(["disruptive"])
  })

  it("are refused where one is no name", () => {
    expect(refusalOf([cellInjecting("tagged", [], ["Not A Tag"])])).toMatch(/^cases\.ts: cell 0: tags/)
  })
})

describe("a source yielding a cell", () => {
  it("is refused where its document injects an action that is no operation, naming the node", () => {
    expect(refusalOf([cellInjecting("killer", [{ id: "i1", op: "inject", action: "node-kill", after: ["s2"] }])])).toBe(
      "cases.ts: cell killer: its inject i1 asks for node-kill, and the rig injects only an operation (operation:<name>)"
    )
  })

  it("is refused where an operation step names an operation that is no name", () => {
    expect(refusalOf([cellInjecting("odd", [{ id: "i1", op: "inject", action: "operation:Restart", after: ["s2"] }])])).toBe(
      "cases.ts: cell odd: its inject i1 asks for operation:Restart, and the rig injects only an operation (operation:<name>)"
    )
  })

  it("is refused where an operation step names no point of its call", () => {
    expect(refusalOf([cellInjecting("pointless", [{ id: "i1", op: "inject", action: "operation:restart-worker" }])])).toBe(
      "cases.ts: cell pointless: its operation step i1 names no point of its call to inject at (after)"
    )
  })

  it("is refused where an operation step states a target, which an operation does not read", () => {
    expect(
      refusalOf([cellInjecting("aimed", [{ id: "i1", op: "inject", action: "operation:restart-worker", target: "w1", after: ["s2"] }])])
    ).toBe("cases.ts: cell aimed: its operation step i1 states a target, which an operation does not read")
  })
})
