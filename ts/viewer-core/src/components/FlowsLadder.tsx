/**
 * The source ladder renderer: one arrow per captured datagram, nothing read
 * into it.
 *
 * Colour carries three facts and only those: the hue is the Call-ID family, the
 * shade the leg inside it, and a leg the case did not select is drawn pale —
 * still readable, still clickable. The tag beads take the fork palette, so a
 * leg answered under two tags reads as two colours under one from-bead.
 */
import type { JSX } from "react"
import { wireForkClass, type WireDialog } from "../flows/dialogs.ts"
import { callIdHueClass, callIdShadeClass, shortCallId } from "../flows/families.ts"
import type { FlowsLayout, FlowsLegHeader, FlowsRow, SocketLifeline } from "../flows/layout.ts"

export interface FlowsLadderProps {
  readonly layout: FlowsLayout
  readonly selectedId?: string | undefined
  readonly onSelect?: ((row: FlowsRow) => void) | undefined
  /** A tag value the reader picked: rows not stating it are drawn faded. */
  readonly selectedTag?: string | undefined
  /** Clicking a bead picks its tag, the way clicking a legend chip does. */
  readonly onSelectTag?: ((tag: string) => void) | undefined
  /** Legs the drawing is narrowed to; empty draws every one of them. */
  readonly legs?: readonly number[] | undefined
  /** Clicking a leg chip in the band narrows the drawing to that leg. */
  readonly onToggleLeg?: ((legIndex: number) => void) | undefined
}

/** Classes every marked element shares: hue, shade, and the case's verdict on the leg. */
const legClasses = (leg: { hueSlot: number; shade: number; selected: boolean }): string =>
  [
    callIdHueClass(leg.hueSlot),
    callIdShadeClass(leg.shade),
    leg.selected ? "fv-picked" : "fv-pale"
  ].join(" ")

const SocketHead = (
  { lifeline, top, boxY, height }: {
    lifeline: SocketLifeline
    top: number
    boxY: number
    height: number
  }
): JSX.Element => (
  <g className={`fv-socket${lifeline.sut ? " fv-socket-sut" : ""}`}>
    <line x1={lifeline.x} x2={lifeline.x} y1={top} y2={height - 8} className="fv-lifeline-line" />
    <rect x={lifeline.x - 96} y={boxY} width={192} height={26} rx={6} className="fv-lifeline-box" />
    <text x={lifeline.x} y={boxY + 17} className="fv-lifeline-label">
      {lifeline.sut ? `SUT ${lifeline.id}` : lifeline.id}
      {lifeline.sut && <title>the system under test: the case's own steps send to and expect from this socket</title>}
    </text>
  </g>
)

/**
 * One leg of the capture: its family colour, its Call-ID, whether the case took
 * it, and — clicked — whether the ladder draws it alone.
 *
 * A filtered-out leg keeps its chip: a control that disappears when used cannot
 * be used to undo itself.
 */
const LegChip = (
  { leg, picked, onToggle }: {
    leg: FlowsLegHeader
    picked: boolean
    onToggle: ((legIndex: number) => void) | undefined
  }
): JSX.Element => (
  <g
    className={[
      "fv-leg-chip",
      legClasses(leg),
      picked ? "fv-leg-chip-on" : "",
      leg.drawn ? "" : "fv-leg-chip-off"
    ].filter(Boolean).join(" ")}
    onClick={onToggle === undefined ? undefined : () => onToggle(leg.legIndex)}
  >
    <title>
      {`leg ${leg.legIndex} · ${leg.callId}${
        onToggle === undefined ? "" : picked ? " — click to draw every leg again" : " — click to draw this leg alone"
      }`}
    </title>
    <rect x={leg.x} y={leg.y} width={leg.width} height={leg.height} rx={5} className="fv-leg-chip-box" />
    <rect x={leg.x} y={leg.y} width={5} height={leg.height} rx={2} className="fv-leg-chip-swatch" />
    <text x={leg.x + 12} y={leg.y + 16} className="fv-leg-chip-label">
      {/* Shorter than the legend's: a chip has less room than a list line. */}
      {`${leg.selected ? "●" : "○"} leg ${leg.legIndex} · ${shortCallId(leg.callId, 20)}`}
    </text>
    <text x={leg.x + leg.width - 8} y={leg.y + 16} className="fv-leg-chip-meta">
      {`${leg.messageCount} msg +${leg.firstAtMs.toFixed(0)}ms`}
    </text>
  </g>
)

/** The pill a bead is drawn in, sized from its text so the ladder stays tight. */
const beadWidth = (text: string): number => text.length * 6.2 + 12

/** The placeholder holding the to-bead's place while no To-tag is stated. */
const NO_TAG = "to—"

const Bead = (
  { centre, y, text, className, title, onPick }: {
    centre: number
    y: number
    text: string
    className: string
    title: string
    onPick: (() => void) | undefined
  }
): JSX.Element => {
  const width = beadWidth(text)
  return (
    <g
      className={className}
      onClick={onPick === undefined ? undefined : (event) => {
        // The bead is a second control on the arrow's own hit area: picking a
        // tag must not also open the datagram the reader clicked through.
        event.stopPropagation()
        onPick()
      }}
    >
      <rect x={centre - width / 2} y={y - 8} width={width} height={16} rx={8} />
      <text x={centre} y={y + 4}>{text}</text>
      <title>{title}</title>
    </g>
  )
}

const fromTitle = (dialog: WireDialog): string =>
  dialog.from === undefined
    ? `leg ${dialog.legIndex} states no originator tag on this message`
    : `${dialog.from.token} = ${dialog.from.value} — the tag of the party that originated leg ${dialog.legIndex}, constant on every message of the leg`

const toTitle = (dialog: WireDialog): string => {
  if (dialog.to === undefined) {
    return `no To-tag yet — nobody has answered leg ${dialog.legIndex}, so no dialog is established`
  }
  const named = `${dialog.to.token} = ${dialog.to.value}`
  return dialog.to.forked
    ? `${named} — fork ${dialog.to.ordinal} of leg ${dialog.legIndex}: the leg answered under several tags, and every later message of this dialog restates this one`
    : `${named} — the answering tag of leg ${dialog.legIndex}'s only dialog`
}

/**
 * The tag beads: the real pair the datagram carries, shortened.
 *
 * Each half is drawn at the end of the arrow belonging to the party whose tag
 * it is — the originator's at the socket that sent the leg's first request —
 * and where the arrow is too short for two beads they collapse into one pill.
 * A fork reads as one from-bead against several to-bead colours.
 */
const TagBeads = (
  { row, onSelectTag }: { row: FlowsRow; onSelectTag: ((tag: string) => void) | undefined }
): JSX.Element | null => {
  const dialog = row.dialog
  if (dialog === undefined || (dialog.from === undefined && dialog.to === undefined)) return null
  const fromText = dialog.from?.short ?? "from—"
  const toText = dialog.to?.short ?? NO_TAG
  const absent = dialog.to === undefined ? " fv-dialog-absent" : ""
  const forkClass = ` ${wireForkClass(dialog.ordinal)}`
  // The originator's tag belongs at the end that first stated it: on a message
  // travelling the leg's own direction that is x1, on a reverse one x2.
  const originAtX1 = row.message.fromTag === dialog.from?.value
  const fromX = originAtX1 ? row.x1 : row.x2
  const toX = originAtX1 ? row.x2 : row.x1
  const inward = toX >= fromX ? 1 : -1
  const from = dialog.from
  const to = dialog.to
  const pickFrom = from === undefined || onSelectTag === undefined
    ? undefined
    : (): void => onSelectTag(from.value)
  const pickTo = to === undefined || onSelectTag === undefined
    ? undefined
    : (): void => onSelectTag(to.value)

  if (beadWidth(fromText) + beadWidth(toText) + 24 > Math.abs(toX - fromX)) {
    const width = beadWidth(`${fromText} · ${toText}`)
    const centre = (fromX + toX) / 2
    return (
      <g
        className={`fv-dialog fv-dialog-pair${absent}${forkClass}`}
        onClick={pickTo === undefined && pickFrom === undefined ? undefined : (event) => {
          event.stopPropagation()
          ;(pickTo ?? pickFrom)?.()
        }}
      >
        <rect x={centre - width / 2} y={row.y - 8} width={width} height={16} rx={8} />
        <text x={centre} y={row.y + 4}>
          <tspan className="fv-dialog-from-tag">{fromText}</tspan>
          <tspan className="fv-dialog-sep">{" · "}</tspan>
          <tspan className="fv-dialog-to-tag">{toText}</tspan>
        </text>
        <title>{`${fromTitle(dialog)}\n${toTitle(dialog)}`}</title>
      </g>
    )
  }

  return (
    <>
      <Bead
        centre={fromX + inward * (beadWidth(fromText) / 2 + 6)}
        y={row.y}
        text={fromText}
        className="fv-dialog fv-dialog-from"
        title={fromTitle(dialog)}
        onPick={pickFrom}
      />
      <Bead
        centre={toX - inward * (beadWidth(toText) / 2 + 6)}
        y={row.y}
        text={toText}
        className={`fv-dialog fv-dialog-to${absent}${forkClass}`}
        title={toTitle(dialog)}
        onPick={pickTo}
      />
    </>
  )
}

/** Whether the row states the tag the reader picked. */
const statesTag = (row: FlowsRow, tag: string | undefined): boolean =>
  tag === undefined || row.dialog?.from?.value === tag || row.dialog?.to?.value === tag

const Datagram = (
  { row, selected, faded, onSelect, onSelectTag }: {
    row: FlowsRow
    selected: boolean
    /** A tag is picked and this row does not state it. */
    faded: boolean
    onSelect: ((row: FlowsRow) => void) | undefined
    onSelectTag: ((tag: string) => void) | undefined
  }
): JSX.Element => {
  const midX = (row.x1 + row.x2) / 2
  const classes = [
    "fv-arrow",
    "fv-datagram",
    legClasses(row),
    row.retx ? "fv-datagram-retx" : "",
    faded ? "fv-datagram-faded" : "",
    selected ? "fv-arrow-selected" : ""
  ].filter(Boolean).join(" ")
  return (
    <g className={classes} onClick={onSelect ? () => onSelect(row) : undefined}>
      <rect
        x={Math.min(row.x1, row.x2) - 4}
        y={row.y - 15}
        width={Math.abs(row.x2 - row.x1) + 8}
        height={28}
        className="fv-arrow-hit"
      />
      <line x1={row.x1} x2={row.x2} y1={row.y} y2={row.y} markerEnd="url(#fv-arrowhead)" />
      <TagBeads row={row} onSelectTag={onSelectTag} />
      <text x={midX} y={row.y - 4} className="fv-arrow-label">{row.label}</text>
      <text x={midX} y={row.y + 11} className="fv-arrow-sub">{row.sublabel}</text>
      <text x={12} y={row.y + 4} className="fv-datagram-time">{`${row.atMs.toFixed(1)}ms`}</text>
    </g>
  )
}

export const FlowsLadder = (
  { layout, selectedId, onSelect, selectedTag, onSelectTag, legs, onToggleLeg }: FlowsLadderProps
): JSX.Element => {
  const picked = new Set(legs ?? [])
  return (
  <svg
    className="fv-ladder fv-source-ladder"
    width={layout.width}
    height={layout.height}
    viewBox={`0 0 ${layout.width} ${layout.height}`}
    role="img"
    aria-label={layout.title}
  >
    <defs>
      <marker id="fv-arrowhead" markerWidth="9" markerHeight="7" refX="8" refY="3.5" orient="auto">
        <polygon points="0 0, 9 3.5, 0 7" />
      </marker>
    </defs>
    {layout.legs.map((leg) => (
      <LegChip
        key={leg.legIndex}
        leg={leg}
        picked={picked.has(leg.legIndex)}
        onToggle={onToggleLeg}
      />
    ))}
    {layout.lifelines.map((lifeline) => (
      <SocketHead
        key={lifeline.id}
        lifeline={lifeline}
        top={layout.lifelineTop}
        boxY={layout.socketBoxY}
        height={layout.height}
      />
    ))}
    {layout.rows.map((row) => (
      <Datagram
        key={row.id}
        row={row}
        selected={row.id === selectedId}
        faded={!statesTag(row, selectedTag)}
        onSelect={onSelect}
        onSelectTag={onSelectTag}
      />
    ))}
  </svg>
  )
}
