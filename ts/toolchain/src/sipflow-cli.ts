/**
 * `sipflow` — capture → flows document, and the RFC-violation census over
 * already-emitted documents.
 *
 * The `--emit-headers` allow-list is the caller's, and it is not optional: a
 * correlation rule that reads a header the document does not project matches
 * nothing, silently. `@sip/contracts` computes the list off the rule file
 * (`Rules.headersNamedBy`); this adapter only passes it on.
 */
import { Flows } from "@sip/contracts"
import type * as Config from "effect/Config"
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import type * as Schema from "effect/Schema"
import { SIPFLOW } from "./binaries.js"
import { type CliFailed, type CliUnavailable, runner } from "./spawn.js"

export interface Interface {
  /** `sipflow --json <capture> [--emit-headers a,b]` → the decoded flows document. */
  readonly flows: (
    capture: string,
    emitHeaders: ReadonlyArray<string>
  ) => Effect.Effect<Flows.FlowsDoc, CliFailed | CliUnavailable | Schema.SchemaError>
  /** `sipflow --schema` → the flows document's own JSON Schema, as text. */
  readonly schema: () => Effect.Effect<string, CliFailed | CliUnavailable>
  /**
   * `sipflow --rfc-census <paths>` → the census report, as text. The report's
   * shape is `sip_pcap::rfc`'s and has no mirror yet, so it rides raw.
   */
  readonly rfcCensus: (paths: ReadonlyArray<string>) => Effect.Effect<string, CliFailed | CliUnavailable>
}

export class Service extends Context.Service<Service, Interface>()("@sip/toolchain/SipflowCli") {}

export const layer = Layer.effect(
  Service,
  Effect.gen(function* () {
    const cli = yield* runner
    const bin = yield* SIPFLOW

    const flows = Effect.fn("SipflowCli.flows")(function* (
      capture: string,
      emitHeaders: ReadonlyArray<string>
    ) {
      const args = ["--json", capture]
      if (emitHeaders.length > 0) args.push("--emit-headers", emitHeaders.join(","))
      return yield* Flows.parseFlows(yield* cli.ok(bin, args))
    })

    const schema = Effect.fn("SipflowCli.schema")(function* () {
      return yield* cli.ok(bin, ["--schema"])
    })

    const rfcCensus = Effect.fn("SipflowCli.rfcCensus")(function* (paths: ReadonlyArray<string>) {
      return yield* cli.ok(bin, ["--rfc-census", paths.join(",")])
    })

    return Service.of({ flows, schema, rfcCensus })
  })
)

/** The binary this service will run, for a caller that wants to say so out loud. */
export const binary: Config.Config<string> = SIPFLOW

export * as SipflowCli from "./sipflow-cli.js"
