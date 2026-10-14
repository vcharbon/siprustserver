/**
 * `Boxing` — how this deployment caps a long-lived child: the cut's case
 * generator, a lane's batch process, each cell's interpreter. A box caps the
 * child's memory below what the host has free, so the box's own kill fires
 * first and takes only that child — above it, the host's OOM killer decides
 * instead, and it does not aim — and its share of the CPUs.
 *
 * What a box is (a cgroup scope, a container, a ulimit) and whether this host
 * can make one now are the deployment's; the pipeline asks before its first
 * long child and boxes each one it starts.
 *
 * The default boxes nothing: a child runs as it is, every host can run it,
 * and a cell execs the interpreter itself.
 */
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"

/** One child to box: its cap, its share of the CPUs, and its command line. */
export interface BoxRequest {
  readonly memoryMax: string
  readonly cpus: number
  readonly command: string
  readonly args: ReadonlyArray<string>
}

/** A command line, as a spawn takes it. */
export interface CommandLine {
  readonly command: string
  readonly args: ReadonlyArray<string>
}

export interface Interface {
  /** The command line that runs `request`'s command inside its box. */
  readonly box: (request: BoxRequest) => CommandLine
  /** Why this host cannot box a child now, naming the fix; `null` where it can. */
  readonly refusal: Effect.Effect<string | null>
  /**
   * The executable a cell runs the interpreter through, which boxes it at the
   * cap its `REPLAY_MEMORY_MAX` states; `undefined` where a cell execs the
   * interpreter itself.
   */
  readonly interpreter: Effect.Effect<string | undefined>
}

export class Service extends Context.Service<Service, Interface>()("@sip/corpus/Boxing") {}

/** The default: no box. */
export const layer = Layer.succeed(
  Service,
  Service.of({
    box: (request) => ({ command: request.command, args: request.args }),
    refusal: Effect.succeed(null),
    interpreter: Effect.succeed(undefined)
  })
)

/** A deployment's own boxing. */
export const layerWith = (boxing: Interface): Layer.Layer<Service> => Layer.succeed(Service, Service.of(boxing))
