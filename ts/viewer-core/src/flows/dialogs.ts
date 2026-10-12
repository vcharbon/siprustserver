/**
 * Wire dialogs of a capture: the REAL tag pair every captured datagram states,
 * shortened to something a ladder row has room for.
 *
 * A capture leg is identified by its Call-ID; the dialogs inside it are told
 * apart by the answering tag. The originator's tag is the From-tag of the leg's
 * dialog-forming request and is constant for the whole leg — but it rides the
 * To header on a reverse-direction request (RFC 3261 §12.2.1.1), so every
 * message is read as ORIGINATOR tag against ANSWERING tag rather than as
 * From against To. A leg stating several answering tags FORKED, and each of
 * them takes its own ordinal so the ladder can colour it.
 *
 * Nothing here parses SIP: the extractor already read the tags into the flows
 * document's summaries, and this module only names what `tag-roster` paired.
 */
import { legTagRoster, shortTag } from "../layout/tag-roster.ts"
import type { FlowsDocument, FlowsLeg, FlowsMessage } from "./parse.ts"

export { shortTag }

/** Which of the two tags of a dialog a token stands for. */
export type WireSide = "from" | "to"

export interface WireTag {
  /** The tag verbatim, as the extractor read it off the wire. */
  readonly value: string
  /** The token a chip states: `from3`, `to3`, and `to3.2` on a fork. */
  readonly token: string
  /** The tag shortened to a ladder bead: head and tail kept, middle elided. */
  readonly short: string
  readonly side: WireSide
  readonly legIndex: number
  /** 1-based over the leg's answering tags; always 1 on the originator's tag. */
  readonly ordinal: number
  /** True on the answering tags of a leg that answered under several of them. */
  readonly forked: boolean
  /** How many drawn messages state this tag. */
  readonly messageCount: number
}

/** The pair one captured datagram states. */
export interface WireDialog {
  readonly legIndex: number
  /** The originator's tag; absent where the leg's first message carried none. */
  readonly from: WireTag | undefined
  /** The answerer's tag; absent until the answerer has stated one. */
  readonly to: WireTag | undefined
  /** The ordinal the fork colour keys on; 0 where no answering tag is stated. */
  readonly ordinal: number
  /** True where the leg answered under several tags. */
  readonly forked: boolean
  /**
   * True where neither of the message's tags is the leg's originator tag, so
   * the pairing is the message's own From/To rather than a derived one.
   */
  readonly unanchored: boolean
}

export interface WireLegDialogs {
  readonly legIndex: number
  readonly from: WireTag | undefined
  /** Every answering tag the leg stated, in the order it first stated them. */
  readonly to: readonly WireTag[]
  readonly forked: boolean
}

export interface WireDialogs {
  /** The pair of every drawn message, by its `leg/msg` coordinate. */
  readonly byMessage: ReadonlyMap<string, WireDialog>
  /** The roster per leg: what the legend lists. */
  readonly byLeg: ReadonlyMap<number, WireLegDialogs>
  /** Every tag some drawn message states, legs in index order, from-tag first. */
  readonly all: readonly WireTag[]
  /** Leg indices stating more than one answering tag. */
  readonly forkedLegs: readonly number[]
}

/** Distinct fork colours before the palette wraps; matches `fv-fork-N` in the CSS. */
export const WIRE_FORK_COUNT = 4

/** The CSS class carrying an answering tag's fork colour. */
export const wireForkClass = (ordinal: number): string =>
  `fv-fork-${ordinal === 0 ? 0 : (ordinal - 1) % WIRE_FORK_COUNT + 1}`

/** The key a message's pair is held under; the source ladder's row id. */
const coordinate = (legIndex: number, msgIndex: number): string => `${legIndex}/${msgIndex}`

/** Derives the wire dialogs of every drawn leg. */
export const wireDialogs = (
  document: Pick<FlowsDocument, "legs">,
  drawn: readonly FlowsMessage[]
): WireDialogs => {
  const messagesByLeg = new Map<number, FlowsMessage[]>()
  for (const message of drawn) {
    const bucket = messagesByLeg.get(message.legIndex)
    if (bucket === undefined) messagesByLeg.set(message.legIndex, [message])
    else bucket.push(message)
  }

  const byMessage = new Map<string, WireDialog>()
  const byLeg = new Map<number, WireLegDialogs>()
  const all: WireTag[] = []
  const forkedLegs: number[] = []

  for (const legIndex of [...messagesByLeg.keys()].sort((left, right) => left - right)) {
    const messages = messagesByLeg.get(legIndex) ?? []
    // The originator is read off the WHOLE leg: a socket filter may hide the
    // dialog-forming request, and the tag of a leg does not change with it.
    const leg: FlowsLeg | undefined = document.legs[legIndex]
    const roster = legTagRoster(messages, leg?.messages ?? messages)
    const { origin, answering, shortOf, count } = roster
    const forked = answering.length > 1
    if (forked) forkedLegs.push(legIndex)

    const fromTag: WireTag | undefined = origin === undefined ? undefined : {
      value: origin,
      token: `from${legIndex}`,
      short: shortOf.get(origin) ?? origin,
      side: "from",
      legIndex,
      ordinal: 1,
      forked: false,
      messageCount: count(origin, "from")
    }
    const toTags: WireTag[] = answering.map((value, index) => ({
      value,
      token: `to${legIndex}${forked ? `.${index + 1}` : ""}`,
      short: shortOf.get(value) ?? value,
      side: "to",
      legIndex,
      ordinal: index + 1,
      forked,
      messageCount: count(value, "to")
    }))
    const toByValue = new Map(toTags.map((tag) => [tag.value, tag]))

    byLeg.set(legIndex, { legIndex, from: fromTag, to: toTags, forked })
    if (fromTag !== undefined) all.push(fromTag)
    all.push(...toTags)

    messages.forEach((message, index) => {
      const pair = roster.pairs[index]!
      const to = pair.to === undefined ? undefined : toByValue.get(pair.to)
      byMessage.set(coordinate(legIndex, message.msgIndex), {
        legIndex,
        // An unanchored message states a tag the leg's originator does not
        // hold; naming it `from<leg>` would claim a dialog identity the
        // capture does not support, so only the anchored side takes a token.
        from: pair.from === origin ? fromTag : undefined,
        to,
        ordinal: to?.ordinal ?? 0,
        forked,
        unanchored: pair.unanchored
      })
    })
  }

  return { byMessage, byLeg, all, forkedLegs }
}
