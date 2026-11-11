/**
 * A job's setup operations (ADR-0060 appendix §3, §7): the operations a named
 * campaign lists as its `setup`, injected in order after provisioning and
 * healed in reverse at the end of the job, whatever ended it: a failed job, an
 * interrupted one, a signal to the runner. An operation whose `inject` failed
 * or was interrupted is healed too: `heal` is correct after a partial inject.
 *
 * A setup name is an operation of the rig path. One that needs parts runs
 * once per distinct set of the part files it needs among the job's lanes, its
 * context the first lane naming that set; one that needs none runs once, with
 * no lane. {@link planOf} refuses, before the job opens, a name the path does
 * not define and one whose parts no lane of the job names.
 */
import * as Effect from "effect/Effect"
import * as Exit from "effect/Exit"
import * as Result from "effect/Result"
import * as path from "node:path"
import type * as Lanes from "./lanes.js"
import { type Refusal, refuse } from "./manifest-file.js"
import type * as Operation from "./operation.js"
import * as OperationRun from "./operation-run.js"
import type * as RigPath from "./rig-path.js"

/** What a job's setup came to: its body's value, and whether an operation left the lab changed. */
export interface Healed<A> {
  readonly value: A
  readonly labLeftChanged: boolean
}

/** The instances of the operation `defined` over `lanes`: one per distinct set of the part files it needs. */
const instancesOf = (
  defined: RigPath.Defined<Operation.Operation>,
  lanes: ReadonlyArray<Lanes.Loaded>
): ReadonlyArray<Lanes.Loaded | undefined> => {
  const needs = defined.manifest.needs ?? []
  if (needs.length === 0) return [undefined]
  const seen = new Map<string, Lanes.Loaded>()
  for (const lane of lanes) {
    const files = needs.map((kind) => lane.parts.get(kind)?.file)
    if (files.some((file) => file === undefined)) continue
    const key = files.join("\n")
    if (!seen.has(key)) seen.set(key, lane)
  }
  return [...seen.values()]
}

/**
 * The instances the setup `names`, stated in `file`, runs over the job's
 * `lanes`, in order, each with its directory under `root`; or the refusal of
 * the first name at fault.
 */
export const planOf = (
  rig: RigPath.RigPath,
  file: string,
  names: ReadonlyArray<string>,
  lanes: ReadonlyArray<Lanes.Loaded>,
  root: string
): Result.Result<ReadonlyArray<OperationRun.Instance>, Refusal> => {
  const planned: Array<OperationRun.Instance> = []
  for (const [index, name] of names.entries()) {
    const defined = rig.operations.get(name)
    if (defined === undefined) return Result.fail(refuse([file], `setup: names ${name}, which no operation of the path defines`))
    const instances = instancesOf(defined, lanes)
    if (instances.length === 0) {
      const needs = (defined.manifest.needs ?? []).join(", ")
      return Result.fail(refuse([file, defined.file], `setup: ${name} needs the parts ${needs}, which no lane of the job names all of`))
    }
    for (const lane of instances) {
      const label = lane === undefined ? name : `${name} for ${lane.id}`
      const dir = path.join(root, `${index + 1}-${lane === undefined ? name : `${name}-${lane.id}`}`)
      planned.push({ operation: defined, label, lane, dir })
    }
  }
  return Result.succeed(planned)
}

/**
 * `body` run inside the setup `instances`: each injected in order, then
 * `body`, then every instance whose inject started healed in reverse order,
 * uninterruptibly, however `body` or an inject ended. A failed inject fails
 * the whole with {@link OperationRun.InjectFailed} once healing is done, and
 * `body` does not run. An interruption interrupts the inject or the body under
 * way, never a heal.
 */
export const around = <A, E, R>(
  instances: ReadonlyArray<OperationRun.Instance>,
  sink: OperationRun.Sink,
  body: Effect.Effect<A, E, R>
): Effect.Effect<Healed<A>, E | OperationRun.InjectFailed, R> =>
  Effect.uninterruptibleMask((restore) =>
    Effect.gen(function*() {
      const started: Array<OperationRun.Instance> = []
      const injected = Effect.forEach(
        instances,
        (instance) => Effect.suspend(() => (started.push(instance), OperationRun.inject(instance, sink))),
        { discard: true }
      )
      const exit = yield* Effect.exit(restore(Effect.andThen(injected, body)))
      let labLeftChanged = false
      for (const instance of [...started].reverse()) {
        if (!(yield* OperationRun.heal(instance, sink))) labLeftChanged = true
      }
      if (Exit.isFailure(exit)) return yield* Effect.failCause(exit.cause)
      return { value: exit.value, labLeftChanged }
    })
  )
