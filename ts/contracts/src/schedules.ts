/**
 * The retransmission schedule table (`pivot-schema schedules`), mirroring
 * `pivot_schema::schedules`'s `ScheduleTable` / `ClassSchedule` — and the
 * table itself, checked in, for a reader with no binary in reach.
 *
 * One schedule, two readers (ADR-0032 X1). `sip-retransmit` owns the RFC 3261
 * §17 / §13.3.1.4 / RFC 3262 §3 ladders; the interpreter paces and counts on
 * it; the cut reads THIS table where it projects a ladder onto a timeline.
 * Nothing here walks T1/T2: {@link SCHEDULES} is data the binary printed, and
 * `@sip/toolchain`'s conformance test holds it byte-equal to what the binary
 * prints now.
 *
 * Each row is one class walked from the original send to its give-up: the
 * wait before rung 1, rung 2, … and no rung that would land at or past the
 * bound — so the list is finite, and a count longer than it is a ladder the
 * RFC has already given up on.
 */
import * as Schema from "effect/Schema"
import { STRICT } from "./strict.js"

/** The stable label `sip_retransmit::Class::as_str` gives each class. */
export const ClassName = Schema.Literals([
  "invite-client",
  "non-invite-client",
  "non-invite-proceeding",
  "cancel-client",
  "invite-server-final",
  "final-2xx",
  "reliable-provisional"
])
export type ClassName = typeof ClassName.Type

/** One class's ladder, walked from the original send to its give-up. */
export const ClassSchedule = Schema.Struct({
  class: ClassName,
  /** The wait before each rung, rung 1 first: every rung landing before the give-up. */
  rung_intervals_ms: Schema.Array(Schema.Int),
  /** The bound from the original send: no rung lands at or past it. */
  give_up_ms: Schema.Int
})
export interface ClassSchedule extends Schema.Schema.Type<typeof ClassSchedule> {}

/** Every class's schedule, one row per class in `Class::ALL` order. */
export const ScheduleTable = Schema.Struct({
  classes: Schema.Array(ClassSchedule)
})
export interface ScheduleTable extends Schema.Schema.Type<typeof ScheduleTable> {}

export const decodeScheduleTable = Schema.decodeUnknownEffect(ScheduleTable, STRICT)
export const decodeScheduleTableSync = Schema.decodeUnknownSync(ScheduleTable, STRICT)

/** The table as `pivot-schema schedules` prints it. */
export const SCHEDULES: ScheduleTable = {
  classes: [
    {
      class: "invite-client",
      give_up_ms: 32000,
      rung_intervals_ms: [500, 1000, 2000, 4000, 8000, 16000]
    },
    {
      class: "non-invite-client",
      give_up_ms: 32000,
      rung_intervals_ms: [500, 1000, 2000, 4000, 4000, 4000, 4000, 4000, 4000, 4000]
    },
    {
      class: "non-invite-proceeding",
      give_up_ms: 32000,
      rung_intervals_ms: [4000, 4000, 4000, 4000, 4000, 4000, 4000]
    },
    {
      class: "cancel-client",
      give_up_ms: 32000,
      rung_intervals_ms: [500, 1000, 2000, 4000, 4000, 4000, 4000, 4000, 4000, 4000]
    },
    {
      class: "invite-server-final",
      give_up_ms: 32000,
      rung_intervals_ms: [500, 1000, 2000, 4000, 4000, 4000, 4000, 4000, 4000, 4000]
    },
    {
      class: "final-2xx",
      give_up_ms: 32000,
      rung_intervals_ms: [500, 1000, 2000, 4000, 4000, 4000, 4000, 4000, 4000, 4000]
    },
    {
      class: "reliable-provisional",
      give_up_ms: 32000,
      rung_intervals_ms: [500, 1000, 2000, 4000, 8000, 16000]
    }
  ]
}

/** The wait before each rung of `cls`'s ladder, rung 1 first, to its give-up. */
export const rungIntervalsMs = (cls: ClassName): ReadonlyArray<number> => {
  const row = SCHEDULES.classes.find((r) => r.class === cls)
  if (row === undefined) throw new Error(`no schedule for class ${cls}`)
  return row.rung_intervals_ms
}

/**
 * The wait before each of a message's `rungs` repeats, rung 1 first: the gaps
 * `stated` where the capture measured them (a step's `retransmit_intervals_ms`,
 * §6.9), otherwise `cls`'s schedule. A count longer than either list repeats
 * the last gap, as `Schedule::exact` does past its list: steady pacing rather
 * than a class the message never chose.
 */
export const rungGapsMs = (
  cls: ClassName,
  rungs: number,
  stated?: ReadonlyArray<number>
): ReadonlyArray<number> => {
  const gaps = stated !== undefined && stated.length > 0 ? stated : rungIntervalsMs(cls)
  return Array.from({ length: rungs }, (_, r) => gaps[Math.min(r, gaps.length - 1)]!)
}
