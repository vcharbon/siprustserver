/**
 * `case <id>…` — every cited form of a case id read back to its call
 * (`@sip/store` `Resolver`): one block per input, the form it was read as,
 * what it came to, and the call or the candidates.
 *
 *   case trace.pcap.gz-case12 case1 run7/case1
 *   case trace.pcap.gz-case12 --cited 2026-09-18
 *   case <Call-ID> --json
 *
 * Exit 0 where every input resolved to one call, 1 where any did not.
 */
import { Resolver } from "@sip/store"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import * as Option from "effect/Option"
import { Argument, Command, Flag } from "effect/unstable/cli"
import type { CliFacts } from "../facts.js"
import { pathFlags } from "../flags.js"
import { exit } from "../runner.js"

export const flags = {
  ...pathFlags,
  ids: Argument.string("id").pipe(
    Argument.withDescription(`A case id in any cited form: ${Resolver.FORMS}`),
    Argument.variadic({ min: 1 })
  ),
  cited: Flag.string("cited").pipe(
    Flag.withDescription("The date the citing ticket wrote the ids (YYYY-MM-DD)"),
    Flag.optional
  ),
  limit: Flag.integer("limit").pipe(Flag.withDescription("Candidates listed per input"), Flag.withDefault(20)),
  json: Flag.boolean("json").pipe(Flag.withDescription("Print the resolutions as JSON"), Flag.withDefault(false))
}

/** One resolution as the lines a reader acts on: a refused call names each token and why. */
export const lines = (resolution: Resolver.Resolution, why: (token: string) => string): ReadonlyArray<string> => [
  `${resolution.input}  [${resolution.form}] ${resolution.status}: ${resolution.note}`,
  ...resolution.calls.flatMap((call) => [
    `  ${call.capture}  ${call.caseId}  key ${call.callKey ?? "-"}  ` +
    (call.prepId === null ? "no cut" : `prep${call.prepId}`) +
    (call.kind === "refused" ? `  refused: ${call.reasons.join(", ")}` : "") +
    (call.standing ? "" : "  (not in the standing cut)"),
    ...(call.kind === "refused"
      ? call.reasons.map((reason) => `    ${reason}: ${why(reason)}`)
      : [])
  ]),
  ...(resolution.total > resolution.calls.length
    ? [`  … ${resolution.total - resolution.calls.length} more`]
    : [])
]

export const command = (facts: CliFacts) =>
  Command.make("case", flags, (input) =>
    Effect.gen(function*() {
      const resolver = yield* Resolver.Service
      const cited = Option.getOrUndefined(input.cited)
      const resolutions = yield* Effect.forEach(input.ids, (id) =>
        resolver.resolve(id, { limit: input.limit, ...(cited === undefined ? {} : { citedOn: cited }) }))
      if (input.json) yield* Console.log(JSON.stringify(resolutions, null, 2))
      else {
        for (const resolution of resolutions) {
          for (const line of lines(resolution, facts.refusalWhy)) yield* Console.log(line)
        }
      }
      yield* exit(resolutions.every((one) => one.status === "resolved") ? 0 : 1)
    })).pipe(Command.withDescription(`Every cited form of a case id, read back to its call\n\n${facts.frontDoors}`))
