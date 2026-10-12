/**
 * The capture legend: the sockets that exchanged the capture, every Call-ID
 * family in it, its colour, and the wire tags each leg answered under.
 *
 * This is the triage key. A source ladder draws the WHOLE capture, so the
 * reader needs to see at a glance which socket is the SUT's, which legs the
 * case took, which it saw and discarded, which legs are one call under a
 * derived Call-ID, and where a leg forked.
 *
 * Every strip here is also a control: a socket narrows the drawing to what it
 * exchanged, a tag lights the messages that state it. Both are the same gesture
 * as "only selected legs" — a click that takes a large capture down to the part
 * being read.
 */
import type { JSX } from "react"
import { wireForkClass, type WireTag } from "../flows/dialogs.ts"
import { callIdHueClass, callIdShadeClass } from "../flows/families.ts"
import type { CaptureSocket, FlowsLayout, FlowsLegHeader } from "../flows/layout.ts"

export interface CaptureLegendProps {
  readonly layout: FlowsLayout
  /** Sockets the drawing is narrowed to; empty draws every one of them. */
  readonly sockets?: readonly string[] | undefined
  readonly onToggleSocket?: ((socket: string) => void) | undefined
  /** The tag the reader picked: its rows stay lit and the rest fade. */
  readonly selectedTag?: string | undefined
  readonly onSelectTag?: ((tag: string) => void) | undefined
  /** Legs the drawing is narrowed to; empty draws every one of them. */
  readonly legs?: readonly number[] | undefined
  readonly onToggleLeg?: ((legIndex: number) => void) | undefined
}

/**
 * The socket strip: every address in the capture, the SUT's named as such.
 *
 * The SUT is not a property of the capture — the flows document is
 * uninterpreted — so it is stated only where the case's own steps name it, and
 * the strip says so rather than guessing when they do not.
 */
const SocketStrip = (
  { sockets, sutSockets, picked, onToggle }: {
    sockets: readonly CaptureSocket[]
    sutSockets: readonly string[]
    picked: ReadonlySet<string>
    onToggle: ((socket: string) => void) | undefined
  }
): JSX.Element => (
  <div className="fv-capture-sockets">
    <span className="fv-capture-rule">
      {sutSockets.length === 0
        ? "SUT unknown: this case cites no capture coordinate, so nothing here names the system under test"
        : `SUT at ${sutSockets.join(", ")} — where the case's steps send to and expect from`}
      {onToggle !== undefined && " · click a socket to draw only what it exchanged"}
    </span>
    <div className="fv-socket-chips">
      {sockets.map((socket) => (
        <button
          key={socket.id}
          type="button"
          className={[
            "fv-socket-chip",
            socket.sut ? "fv-socket-chip-sut" : "",
            picked.has(socket.id) ? "fv-socket-chip-on" : ""
          ].filter(Boolean).join(" ")}
          disabled={onToggle === undefined}
          title={`${socket.id}: ${socket.messageCount} message(s)${
            socket.sut ? " · the SUT's own socket" : ""
          }`}
          onClick={onToggle === undefined ? undefined : () => onToggle(socket.id)}
        >
          {socket.sut && <b className="fv-socket-badge">SUT</b>}
          <code>{socket.id}</code>
          <span className="fv-capture-meta">{socket.messageCount}</span>
        </button>
      ))}
    </div>
  </div>
)

/** One tag chip: its token, its shortened value, and how much of the leg states it. */
const TagChip = (
  { tag, selected, onSelect }: {
    tag: WireTag
    selected: boolean
    onSelect: ((tag: string) => void) | undefined
  }
): JSX.Element => (
  <button
    key={tag.token}
    type="button"
    className={[
      "fv-tag-chip",
      tag.side === "from" ? "fv-tag-chip-from" : wireForkClass(tag.ordinal),
      selected ? "fv-tag-chip-on" : ""
    ].filter(Boolean).join(" ")}
    disabled={onSelect === undefined}
    title={`${tag.token} = ${tag.value} — ${tag.messageCount} message(s)${
      tag.forked ? `, fork ${tag.ordinal} of leg ${tag.legIndex}` : ""
    }`}
    onClick={onSelect === undefined ? undefined : () => onSelect(tag.value)}
  >
    <b>{tag.token}</b>
    <code>{tag.short}</code>
    <span className="fv-capture-meta">{tag.messageCount}</span>
  </button>
)

/**
 * A leg's tags, on the line under it: one from-chip and one to-chip per
 * answering tag. Several to-chips IS the fork, and each takes its own colour —
 * the same colours the beads on the ladder carry.
 */
const TagLine = (
  { layout, legIndex, selectedTag, onSelectTag }: {
    layout: FlowsLayout
    legIndex: number
    selectedTag: string | undefined
    onSelectTag: ((tag: string) => void) | undefined
  }
): JSX.Element | null => {
  const dialogs = layout.dialogs.byLeg.get(legIndex)
  if (dialogs === undefined || (dialogs.from === undefined && dialogs.to.length === 0)) return null
  return (
    <div className="fv-capture-tags">
      {dialogs.from !== undefined && (
        <TagChip tag={dialogs.from} selected={dialogs.from.value === selectedTag} onSelect={onSelectTag} />
      )}
      {dialogs.to.map((tag) => (
        <TagChip key={tag.token} tag={tag} selected={tag.value === selectedTag} onSelect={onSelectTag} />
      ))}
      {dialogs.to.length === 0 && <span className="fv-capture-meta">unanswered: no To-tag on this leg</span>}
      {dialogs.forked && <span className="fv-capture-mark">forked ×{dialogs.to.length}</span>}
    </div>
  )
}

const LegLine = (
  { leg, layout, selectedTag, onSelectTag, picked, onToggleLeg }: {
    leg: FlowsLegHeader
    layout: FlowsLayout
    selectedTag: string | undefined
    onSelectTag: ((tag: string) => void) | undefined
    picked: boolean
    onToggleLeg: ((legIndex: number) => void) | undefined
  }
): JSX.Element => (
  <li
    key={leg.legIndex}
    className={`fv-capture-leg ${callIdHueClass(leg.hueSlot)} ${callIdShadeClass(leg.shade)} ${
      leg.selected ? "fv-picked" : "fv-pale"
    }`}
  >
    <div className="fv-capture-leg-head">
      <span className="fv-capture-swatch" aria-hidden="true" />
      <button
        type="button"
        className={`fv-leg-button${picked ? " fv-leg-button-on" : ""}`}
        disabled={onToggleLeg === undefined}
        title={picked ? "drawing this leg alone — click to draw every leg again" : "draw only this leg"}
        onClick={onToggleLeg === undefined ? undefined : () => onToggleLeg(leg.legIndex)}
      >
        leg {leg.legIndex}
      </button>
      <code title={leg.callId}>{leg.shortCallId}</code>
      <span className="fv-capture-mark">{leg.selected ? "● selected" : "○ discarded"}</span>
      <span className="fv-capture-meta">
        {leg.messageCount} msg · first +{leg.firstAtMs.toFixed(0)}ms
        {leg.finalStatus === undefined ? "" : ` · final ${leg.finalStatus}`}
        {leg.derivedFrom === undefined ? "" : ` · derived from leg ${leg.derivedFrom}`}
        {leg.rideAlong === undefined ? "" : ` · ${leg.rideAlong}`}
      </span>
    </div>
    <TagLine
      layout={layout}
      legIndex={leg.legIndex}
      selectedTag={selectedTag}
      onSelectTag={onSelectTag}
    />
  </li>
)

/** One block per Call-ID family, its legs inside it. */
export const CaptureLegend = (
  { layout, sockets, onToggleSocket, selectedTag, onSelectTag, legs, onToggleLeg }: CaptureLegendProps
): JSX.Element => {
  const drawn = new Map(layout.legs.map((leg) => [leg.legIndex, leg]))
  const picked = new Set(sockets ?? [])
  const pickedLegs = new Set(legs ?? [])
  return (
    <div className="fv-capture-legend">
      <SocketStrip
        sockets={layout.sockets}
        sutSockets={layout.sutSockets}
        picked={picked}
        onToggle={onToggleSocket}
      />
      <p className="fv-capture-rule">
        hue = Call-ID family, shade = leg within it (a derived Call-ID keeps its base's hue)
        {layout.hasSelection
          ? " · full colour = selected by this case, pale = seen and discarded"
          : " · this document cites no capture coordinate, so every leg is drawn selected"}
        {" · a leg's chips are its REAL tags, shortened: one "}
        <code>from</code>
        {" against one "}
        <code>to</code>
        {" per dialog, so several to-chips under one from-chip is a fork"}
        {onSelectTag !== undefined && " · click a tag to light only the messages that state it"}
        {onToggleLeg !== undefined && " · click a leg to draw that leg alone"}
        {layout.hiddenMessages > 0 && ` · ${layout.hiddenMessages} messages are filtered out of this drawing`}
      </p>
      {layout.families.families.map((family) => {
        const legs = family.legs.map((leg) => drawn.get(leg)).filter((leg): leg is FlowsLegHeader => leg !== undefined)
        if (legs.length === 0) return null
        return (
          <div key={family.index} className={`fv-capture-family ${callIdHueClass(family.hueSlot)}`}>
            <h4>
              <span className="fv-capture-swatch fv-shade-0" aria-hidden="true" />
              {family.baseCallId}
              {legs.length > 1 && <em> · {legs.length} legs</em>}
            </h4>
            <ul>
              {legs.map((leg) => (
                <LegLine
                  key={leg.legIndex}
                  leg={leg}
                  layout={layout}
                  selectedTag={selectedTag}
                  onSelectTag={onSelectTag}
                  picked={pickedLegs.has(leg.legIndex)}
                  onToggleLeg={onToggleLeg}
                />
              ))}
            </ul>
          </div>
        )
      })}
    </div>
  )
}
