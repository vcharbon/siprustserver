/**
 * `${VAR}` in a part's strings: a set variable's value, a default where it is
 * unset or empty, a `$` outside the form kept, and a reference to an unset
 * variable refusing the value, naming the dotted key and the variable.
 */
import * as Result from "effect/Result"
import { describe, expect, it } from "vitest"
import { expandAll, expandText } from "../src/expand.js"

describe("a part's strings", () => {
  it("take a set variable's value, a default where it is unset or empty, and keep a bare $", () => {
    expect(expandText("a${X}b${Y:-d}c$Z", { X: "1" })).toEqual(Result.succeed("a1bdc$Z"))
    expect(expandText("${X:-d}", { X: "" })).toEqual(Result.succeed("d"))
  })

  it("name the first unset variable a string refers to", () => {
    expect(expandText("${A}${B}", {})).toEqual(Result.fail("A"))
  })

  it("expand at any depth, refused by an unset variable naming the dotted key", () => {
    expect(expandAll({ a: { b: ["${X}", 2] } }, { X: "x" })).toEqual(Result.succeed({ a: { b: ["x", 2] } }))
    expect(expandAll({ a: { b: ["ok", "${NOPE}"] } }, {})).toEqual(Result.fail("a.b.1: ${NOPE} is not set"))
  })
})
