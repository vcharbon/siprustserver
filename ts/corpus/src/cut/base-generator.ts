/**
 * The base's CASE GENERATOR, the one a deployment names as its `Tools`
 * generator where it states no cut rule of its own:
 *
 *   tsx src/cut/base-generator.ts --work-list <file> [--manifest <file>] [--census-report <file>]
 *     [--call-id-derivation none|counter-prefix] [--correlation-rules <file>]
 *
 * `--call-id-derivation` names how the system under test derives the Call-ID
 * of a leg it dials, which joins its legs into one call: none (the default),
 * or the upstream B2BUA's `<n>-<base>`.
 *
 * `--correlation-rules` names the rule file whose `header-pair` rules join
 * legs beside the derivation (`leg-rules.ts`); a flows document that does not
 * project a header they read (`Tools.Interface.emitHeaders`) is not cut. Where
 * none is named, the base's default set (`data/default.rules.json`, `X-CID`
 * against the caller leg's Call-ID, as Asterisk dialplans set it) joins legs,
 * and a document that does not project its headers is cut without it, the
 * capture's `[cut]` line naming the rule skipped. A rule file that does not
 * read, or whose patterns do not compile, is refused whole, exit 2 naming it.
 *
 * Each capture of the work list (`cut-contract.ts`) is cut by the neutral cut
 * (`neutral-cut.ts`) from the one flows document its source directory holds,
 * and its line (`CaptureCut`) appended to the manifest once it is done: cut,
 * or not cut with the reason. The census report is accepted and not read: the
 * neutral policy declares nothing a census would decide.
 *
 * Exit 0 once every capture has its line; 2 where the work list or the rule
 * file does not read.
 */
import { NodeRuntime, NodeServices } from "@effect/platform-node"
import { Tokens } from "@sip/contracts"
import { ANY_LANE } from "@sip/driver/lanes"
import { CaseAssembler, type HeaderPairJoin, LogicExtractor, Policy } from "@sip/pipeline"
import { type CallIdDerivation, counterPrefixed, neverDerives } from "@sip/pipeline/derivation"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Layer from "effect/Layer"
import * as Option from "effect/Option"
import { Command, Flag } from "effect/unstable/cli"
import { type CaptureCut, decodeWorkList } from "./cut-contract.js"
import { DEFAULT_RULES, legRulesOf } from "./leg-rules.js"
import { cutCapture } from "./neutral-cut.js"

const flags = {
  workList: Flag.string("work-list").pipe(Flag.withDescription("A JSON work list: [{ capture, srcDir, outDir }, ...]")),
  manifest: Flag.string("manifest").pipe(
    Flag.withDescription("Where each capture's outcome is appended as it finishes"),
    Flag.optional
  ),
  censusReport: Flag.string("census-report").pipe(Flag.withDescription("Accepted; the neutral cut reads no census"), Flag.optional),
  derivation: Flag.choice("call-id-derivation", ["none", "counter-prefix"]).pipe(
    Flag.withDescription("How the system under test derives a dialled leg's Call-ID: none, or `<n>-<base>` (the upstream B2BUA's)"),
    Flag.withDefault("none")
  ),
  rules: Flag.string("correlation-rules").pipe(
    Flag.withDescription(
      "A rule file whose header-pair rules join legs beside the derivation, every header they read projected " +
        "or the capture is not cut; where none is named, the base's default set (X-CID against the caller leg's " +
        "Call-ID, as Asterisk dialplans set it), skipped on a capture not projecting X-CID"
    ),
    Flag.optional
  )
}

/**
 * The leg rules `named` states, required of every document; the default set
 * where none is named, skipped where a document does not project its headers.
 */
const legRulesNamed = (named: Option.Option<string>) =>
  Option.match(named, {
    onNone: () => Effect.map(legRulesOf(DEFAULT_RULES), (rules): HeaderPairJoin.LegRules => ({ rules, unprojected: "skip" })),
    onSome: (file) => Effect.map(legRulesOf(file), (rules): HeaderPairJoin.LegRules => ({ rules, unprojected: "require" }))
  })

/** The Call-ID derivations by the name `--call-id-derivation` gives them. */
const DERIVATIONS: Readonly<Record<"none" | "counter-prefix", CallIdDerivation>> = { none: neverDerives, "counter-prefix": counterPrefixed }

/**
 * The neutral policy under `derives` and `legRules`, every case it assembles
 * replayable on every lane: the base states no lane a case cannot run on. The
 * joins are the policy's, so the cut and the assembly of a capture join its
 * legs alike.
 */
const assemblerUnder = (derives: CallIdDerivation, legRules: HeaderPairJoin.LegRules) =>
  CaseAssembler.layer.pipe(
    Layer.provide(
      LogicExtractor.layerWith(
        Policy.policyWith({ derives, legRules, lanes: () => ({ [ANY_LANE]: Tokens.laneVerdictToken({ _tag: "ok" }) }) })
      )
    )
  )

/** What a capture's line says of a default rule its flows document left out, and the two remedies. */
const skippedLine = (skipped: HeaderPairJoin.SkippedRule): string =>
  `the default leg rule "${skipped.rule}" is skipped: the flows document does not project ${skipped.missing.join(", ")}; ` +
  "project it through the deployment's Tools.emitHeaders, or name a rule set with --correlation-rules"

/** One capture's line, appended to `manifest` where one is named. */
const said = (manifest: Option.Option<string>, line: CaptureCut) =>
  Effect.gen(function*() {
    yield* Console.log(`[cut] ${line.capture}: ${line.cut ? "cut" : `not cut: ${line.error}`}`)
    if (Option.isSome(manifest)) {
      yield* (yield* FileSystem.FileSystem).writeFileString(manifest.value, `${JSON.stringify(line)}\n`, { flag: "a" })
    }
  })

const command = Command.make("base-generator", flags, (input) =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const read = yield* fs.readFileString(input.workList).pipe(
      Effect.flatMap((text) => Effect.try(() => decodeWorkList(JSON.parse(text)))),
      Effect.result
    )
    if (read._tag === "Failure") {
      yield* Console.error(`base-generator: the work list ${input.workList} does not read: ${String(read.failure)}`)
      process.exitCode = 2
      return
    }
    const rules = yield* legRulesNamed(input.rules).pipe(Effect.result)
    if (rules._tag === "Failure") {
      yield* Console.error(`base-generator: ${rules.failure.message}`)
      process.exitCode = 2
      return
    }
    yield* Effect.gen(function*() {
      for (const work of read.success) {
        const done = yield* cutCapture(work).pipe(Effect.result)
        const line: CaptureCut = done._tag === "Success"
          ? { capture: work.capture, cut: true }
          : { capture: work.capture, cut: false, error: done.failure.message }
        if (done._tag === "Success") {
          for (const skipped of done.success.skipped) yield* Console.log(`[cut] ${work.capture}: ${skippedLine(skipped)}`)
          for (const record of done.success.refused) yield* Console.log(`[cut] ${record.caseId}: EXCLUDED ${record.reason}`)
        }
        yield* said(input.manifest, line)
      }
    }).pipe(Effect.provide(assemblerUnder(DERIVATIONS[input.derivation], rules.success)))
  }))

Command.run(command, { version: "0.0.0" }).pipe(
  Effect.provide(NodeServices.layer),
  NodeRuntime.runMain
)
