/**
 * An id flag or argument reads every form the store cites an id in, and
 * refuses anything else at parse time, naming the forms. Parsed through a
 * command's own flag set, as an operator's line is.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import { Command, Flag } from "effect/unstable/cli"
import { describe, expect, it } from "vitest"
import * as IdFlags from "../src/id-flags.js"

/** What a line parses to: the ids read, or the parse's failure. */
const parse = (argv: ReadonlyArray<string>) => {
  let seen: { readonly prep: number | undefined; readonly run: number } | undefined
  const command = Command.make(
    "t",
    { prep: IdFlags.flag("prep", "prep").pipe(Flag.optional), run: IdFlags.argument("run", "run") },
    (input) =>
      Effect.sync(() => {
        seen = { prep: input.prep._tag === "Some" ? input.prep.value : undefined, run: input.run }
      })
  )
  return Effect.runPromise(
    Command.runWith(command, { version: "0.0.0", renderErrors: false })(argv).pipe(
      Effect.map(() => seen),
      Effect.provide(NodeServices.layer),
      Effect.catch((error) =>
        Effect.succeed(
          error._tag === "ShowHelp" ? error.errors.map((one) => one.message).join("\n") : String(error)
        )
      )
    )
  )
}

describe("IdFlags", () => {
  it("reads N, kindN and @kind:N alike", async () => {
    expect(await parse(["--prep", "prep110", "@run:7"])).toEqual({ prep: 110, run: 7 })
    expect(await parse(["--prep", "110", "run7"])).toEqual({ prep: 110, run: 7 })
  })

  it("refuses an id of another kind, naming the forms", async () => {
    expect(await parse(["--prep", "run110", "7"])).toContain("110, prep110, @prep:110")
  })
})
