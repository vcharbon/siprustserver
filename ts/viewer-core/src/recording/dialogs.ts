/**
 * Dialog tags of a run recording: the REAL tag pair every recorded datagram
 * states, named with the tokens the scenario ladder uses.
 *
 * The scenario names dialogs by the early ids it declares, because a pivot
 * stores no tag. A recording holds the wire, so its tokens are aliases of tag
 * VALUES: `fromA1` is the leg's originator tag, `toA1` the first answering tag
 * the leg stated, `toA2` the second — so a message answered under a tag the
 * leg had not stated before is drawn under a new token, and the change is on
 * the ladder. The pairing itself is `tag-roster`'s, shared with the capture
 * ladder.
 */
import type { DialogTag, DialogToken, TagOwner } from "../layout/dialogs.ts"
import { legTagRoster } from "../layout/tag-roster.ts"
import type { RecordedMessage, RunRecording } from "./parse.ts"

export interface RecordedDialogs {
  /** The pair of every recorded message that stated a tag, by `<leg>#<seq>`. */
  readonly byMessage: ReadonlyMap<string, DialogTag>
  /** Every token some message states, legs in recording order, from-token first. */
  readonly all: readonly DialogToken[]
  /** Leg ids that answered under several tags. */
  readonly forkedLegs: readonly string[]
  /** Leg ids whose recording stated an originator tag, so their tokens are the wire's. */
  readonly legs: ReadonlySet<string>
}

/** The id the recording layout gives a message's event. */
export const recordedEventId = (message: Pick<RecordedMessage, "leg" | "seq">): string =>
  `${message.leg}#${message.seq}`

/**
 * Which lifeline originated a leg: the actor when the leg's first message
 * left it, the SUT when it arrived. A document's `dir` says the same thing
 * where the bundle carries one.
 */
const originatorOf = (
  first: RecordedMessage | undefined,
  declared: TagOwner | undefined
): TagOwner => declared ?? (first?.dir === "in" ? "sut" : "actor")

/**
 * Names the tags of every recorded leg.
 *
 * `declaredOriginator` states, per leg, which side originated it when the
 * bundle's document knows; otherwise the recording's own first message on the
 * leg decides. A message with no From-tag at all stated no dialog and takes no
 * pair; a leg with no originator tag has no tokens and is left to the caller's
 * fallback.
 */
export const recordedDialogTags = (
  recording: RunRecording,
  declaredOriginator: ReadonlyMap<string, TagOwner> = new Map()
): RecordedDialogs => {
  const byMessage = new Map<string, DialogTag>()
  const all: DialogToken[] = []
  const forkedLegs: string[] = []
  const legs = new Set<string>()

  for (const legId of recording.legs) {
    const messages = recording.messages
      .filter((message) => message.leg === legId)
      .sort((left, right) => left.seq - right.seq)
    const roster = legTagRoster(messages)
    const { origin, answering } = roster
    if (origin === undefined) continue
    legs.add(legId)
    const forked = answering.length > 1
    if (forked) forkedLegs.push(legId)

    const originator = originatorOf(messages[0], declaredOriginator.get(legId))
    const from: DialogToken = {
      token: `from${legId}1`,
      side: "from",
      legId,
      owner: originator,
      earlyId: undefined,
      ordinal: 1,
      forked: false,
      value: origin
    }
    const toByValue = new Map<string, DialogToken>(
      answering.map((value, index) => [value, {
        token: `to${legId}${index + 1}`,
        side: "to",
        legId,
        owner: originator === "actor" ? "sut" : "actor",
        earlyId: undefined,
        ordinal: index + 1,
        forked,
        value
      }])
    )
    all.push(from, ...toByValue.values())

    messages.forEach((message, index) => {
      const pair = roster.pairs[index]!
      // A message stating no tag on either side rides no dialog the wire can
      // name; the caller falls back to what the scenario says of it.
      if (pair.from === undefined && pair.to === undefined) return
      const to = pair.to === undefined ? undefined : toByValue.get(pair.to)
      byMessage.set(recordedEventId(message), {
        legId,
        from,
        to,
        token: to === undefined ? from.token : `${from.token} · ${to.token}`,
        earlyId: undefined,
        ordinal: to?.ordinal ?? 0,
        forked
      })
    })
  }

  return { byMessage, all, forkedLegs, legs }
}
