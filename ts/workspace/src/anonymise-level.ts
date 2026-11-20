/**
 * The ANONYMISATION LEVEL a capture enters a workspace's pcap root at, by its
 * inbox take: `keep-all-ips` (the default: numbers, names, hosts and charging
 * ids faked, every address kept), `keep-sut` (public peers faked, the SUT
 * ranges and the private addresses kept), `shareable` (every address faked: a
 * copy fit to leave the network, whose SUT the cut no longer finds, so it
 * yields no case) and `none` (the capture lands as dropped: a lab capture
 * holding nothing private). The configuration file's `[anonymise] level`
 * states a workspace's default.
 */
import * as Schema from "effect/Schema"

export const LEVELS = ["keep-all-ips", "keep-sut", "shareable", "none"] as const

export type Level = (typeof LEVELS)[number]

/** The level of a capture nothing states one for. */
export const DEFAULT: Level = "keep-all-ips"

export const Level = Schema.Literals(LEVELS)

/** Whether `text` names a level. */
export const isLevel = (text: string): text is Level => (LEVELS as ReadonlyArray<string>).includes(text)

/** The configuration file's `[anonymise]` section. */
export const Section = Schema.Struct({
  /** The level a capture copied into the inbox is taken at where nothing beside it states one. */
  level: Schema.optionalKey(Level)
})
