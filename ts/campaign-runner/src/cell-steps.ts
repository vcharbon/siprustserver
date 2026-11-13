/**
 * The operation steps of one cell on a hosted lane (upstream ADR-0060
 * appendix §7, `@sip/rig` `OperationStep`): each inject its run asks is the
 * operation its lane binds by that name, run with the cell's context and the
 * binding's parameters (`@sip/rig` `OperationRun`); once its cell ended, every
 * step whose inject started is healed in reverse, a failed inject's too, each
 * phase an event of the job.
 *
 * The lane's managed system is its orchestrator's to stand: where a step's
 * inject stopped it, it is stood again before the inject is answered, so what
 * the call sends next reaches the system as it restarted; each heal attempt
 * stands it again where it is still down, before its probe. Only the death of
 * the process an inject of an operation that `stops` the system named stopped
 * is the step's; any other death found there stays the batch's (the system's
 * `died`, its dirty end) and is the event `system-died` of the step, its
 * exit stated.
 */
import type { Injector, SutHandle } from "@sip/corpus/replay/batch-protocol"
import { type Context, type Lanes, OperationRun, OperationStep } from "@sip/rig"
import * as Effect from "effect/Effect"
import * as path from "node:path"

/** One cell's steps: what performs its injects, and its heals once it ended. */
export interface Steps {
  readonly injector: Injector
  /** Every step whose inject started healed, in reverse, uninterruptibly: the labels of those that left the lab changed. */
  readonly healed: Effect.Effect<ReadonlyArray<string>>
}

/**
 * The steps of `cell` on `lane`, each instance's directory under `dir`, its
 * events to `sink`, the lane's system `sut` stood again where a step left it
 * down.
 */
export const stepsOf = (
  lane: Lanes.Loaded,
  cell: Context.Planned,
  dir: string,
  sink: OperationRun.Sink,
  sut: SutHandle | undefined
): Steps => {
  const started: Array<OperationRun.Instance> = []
  const restand = sut?.restand
  const stoodAgain = (instance: OperationRun.Instance, claim?: number): Effect.Effect<void, string> =>
    restand === undefined
      ? Effect.void
      : Effect.flatMap(restand(claim), (restood) =>
        restood === undefined || restood.claimed
          ? Effect.void
          : OperationRun.emit(sink, instance, "system-died", `no step stopped it: ${restood.exit}`))
  const injector: Injector = (ask) =>
    Effect.suspend(() => {
      const name = OperationStep.operationOf(ask.action)
      if (name === undefined) return Effect.fail(`${ask.action} asks for no operation`)
      const operation = lane.operations.get(name)
      if (operation === undefined) return Effect.fail(`the lane ${lane.id} binds no operation ${name}`)
      const params = lane.operationParams?.get(name)
      const instance: OperationRun.Instance = {
        operation,
        label: `${name} for ${cell.id}`,
        lane,
        dir: path.join(dir, `${cell.id}-${ask.node}`),
        cell,
        ...(params === undefined ? {} : { params })
      }
      started.push(instance)
      return OperationRun.inject(instance, sink, (stopped) => stoodAgain(instance, stopped)).pipe(
        Effect.mapError((failed) => failed.message)
      )
    })
  const healed = Effect.suspend(() =>
    Effect.map(
      Effect.forEach([...started].reverse(), (instance) =>
        Effect.map(OperationRun.heal(instance, sink, stoodAgain(instance)), (recovered) => (recovered ? [] : [instance.label]))),
      (left) => left.flat()
    )
  )
  return { injector, healed }
}

/** What a batch's end states of the steps `labels` names that left the lab changed. */
export const leftChangedOf = (labels: ReadonlyArray<string>): string | undefined =>
  labels.length === 0
    ? undefined
    : `the step ${labels.join(", ")} left the lab changed: ${OperationRun.HEAL_ATTEMPTS} heal attempts failed`
