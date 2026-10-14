/**
 * `Tools` — the binaries the pipeline runs, as this deployment builds them:
 * the flows extractor (`sipflow`: read, enrich, index), the case generator a
 * cut launches, the replay interpreter every cell execs, and the wrapper the
 * bench measures a phase under; with the source paths whose content decides
 * what an extraction and a cut produce.
 *
 * A tool is brought up to date before it is used, never only-if-absent: a run
 * records the code that made it, so yesterday's binary under today's tree
 * would file its answer under the wrong code. How it is built is the
 * deployment's; the pipeline only asks for the path once it is current.
 *
 * The default knows no tool and refuses each by name when a phase reaches for
 * it.
 */
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as Schema from "effect/Schema"

/** The tools a phase asks for by name. */
export type Tool = "extractor" | "interpreter"

/** The tool cannot be brought up to date; the message is why. */
export class ToolUnavailable extends Schema.TaggedError<ToolUnavailable>()("Corpus.ToolUnavailable", {
  tool: Schema.String,
  detail: Schema.String
}) {
  override get message(): string {
    return `${this.tool}: ${this.detail}`
  }
}

/** Who hears whose build a caller waits for: a job writes it on its row. */
export type Watch = (holder: string | undefined) => Effect.Effect<void>

/** The source paths, relative to `root`, whose content decides a phase's output. */
export interface Decides {
  readonly root: string
  readonly extraction: ReadonlyArray<string>
  readonly cut: ReadonlyArray<string>
}

/**
 * The case generator a cut launches: its command line before the cut's own
 * arguments (`--work-list`, `--manifest`, `--census-report`, the cut
 * contract's), and the directory it runs in. It runs as a node process, its
 * heap sized to the cut's memory scope.
 */
export interface Generator {
  readonly argv: ReadonlyArray<string>
  readonly cwd: string
}

export interface Interface {
  /** The tool's path once it is current; `watch` hears who holds the build while this waits for it. */
  readonly ready: (tool: Tool, watch?: Watch) => Effect.Effect<string, ToolUnavailable>
  /** The interpreter inside a memory scope of its own (`REPLAY_MEMORY_MAX`): what a cell execs. */
  readonly boxedInterpreter: Effect.Effect<string, ToolUnavailable>
  readonly generator: Effect.Effect<Generator, ToolUnavailable>
  /** The wrapper a measured phase runs under: wall time, peak RSS of its largest process, exit. */
  readonly measure: Effect.Effect<string, ToolUnavailable>
  /** Milliseconds this process has spent compiling, waits for another process's build left out. */
  readonly compileMs: Effect.Effect<number>
  readonly decides: Effect.Effect<Decides, ToolUnavailable>
}

export class Service extends Context.Service<Service, Interface>()("@sip/corpus/Tools") {}

const unknown = (tool: string) =>
  Effect.fail(
    new ToolUnavailable({
      tool,
      detail: "no tools are composed — substitute Tools.layerWith to say how this deployment builds it"
    })
  )

/** The default: no tool is known, each refused when a phase reaches for it. */
export const layer = Layer.succeed(
  Service,
  Service.of({
    ready: (tool) => unknown(tool),
    boxedInterpreter: unknown("interpreter"),
    generator: unknown("generator"),
    measure: unknown("measure"),
    compileMs: Effect.succeed(0),
    decides: unknown("decides")
  })
)

/** A deployment's own tools. */
export const layerWith = (tools: Interface): Layer.Layer<Service> => Layer.succeed(Service, Service.of(tools))
