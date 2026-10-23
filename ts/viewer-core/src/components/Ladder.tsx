/** The ladder renderer: a layout model drawn as SVG. Geometry comes in; this file only draws. */
import type { JSX } from "react"
import { callSlotClass } from "../layout/calls.ts"
import type { DialogTag } from "../layout/dialogs.ts"
import type { Block, BlockBranch, LadderLayout, LadderEvent, Lifeline, MessageEvent } from "../layout/model.ts"
import { isMessageEvent } from "../layout/model.ts"
import { decorationBadges } from "./decoration-text.ts"

export type EventStatus = "matched" | "missing" | "extra"

/** What a ruling says about ONE row, in the weight it says it with. */
export type MarkTone = "failure" | "declared" | "abandoned" | "violation" | "tolerated" | "informative"

/**
 * A ruling drawn ON the row it was made at. The renderer knows nothing of
 * verdicts: a caller that has one turns it into text and a tone, and the row
 * carrying the message states it instead of a panel elsewhere on the page.
 */
export interface LadderMark {
  readonly tone: MarkTone
  readonly text: string
  /** The full finding, on hover. */
  readonly title: string
}

export interface LadderProps {
  readonly layout: LadderLayout
  readonly selectedId?: string | undefined
  readonly onSelect?: ((event: LadderEvent) => void) | undefined
  /** Per-event compare verdict, colouring the arrows in a compare view. */
  readonly statusById?: Readonly<Record<string, EventStatus>> | undefined
  /** Per-event ruling, banding the row and naming the finding beside it. */
  readonly marksById?: Readonly<Record<string, LadderMark>> | undefined
}

/** Calls are tinted only where there is more than one to tell apart. */
const callTint = (layout: LadderLayout, slot: number | undefined): string =>
  layout.calls.length > 1 && slot !== undefined ? ` fv-tinted ${callSlotClass(slot)}` : ""

const LifelineHead = (
  { lifeline, height, tint }: { lifeline: Lifeline; height: number; tint: string }
): JSX.Element => (
  <g className={`fv-lifeline fv-lifeline-${lifeline.kind}${tint}`}>
    <line x1={lifeline.x} x2={lifeline.x} y1={64} y2={height - 8} className="fv-lifeline-line" />
    <rect x={lifeline.x - 78} y={12} width={156} height={44} rx={6} className="fv-lifeline-box" />
    <text x={lifeline.x} y={30} className="fv-lifeline-label">{lifeline.label}</text>
    <text x={lifeline.x} y={44} className="fv-lifeline-sub">
      {[lifeline.actorKind, lifeline.sublabel].filter(Boolean).join(" · ")}
    </text>
    {lifeline.legs.length > 0 && (
      <text x={lifeline.x} y={58} className="fv-lifeline-sub">
        {`legs ${lifeline.legs.join(", ")}${lifeline.callIds.length > 0 ? ` · ${lifeline.callIds.join(", ")}` : ""}`}
      </text>
    )}
    {lifeline.background.length > 0 && (
      <title>{`background: ${lifeline.background.join("; ")}`}</title>
    )}
  </g>
)

/** A full-width rule with one centred caption: the "or" and "rejoin" marks. */
const BandMark = (
  { x0, x1, y, text, className }: {
    x0: number
    x1: number
    y: number
    text: string
    className: string
  }
): JSX.Element => {
  const centre = (x0 + x1) / 2
  const width = text.length * 6.6 + 28
  return (
    <g className={className}>
      <line x1={x0 - 30} x2={x1 + 30} y1={y} y2={y} />
      <rect x={centre - width / 2} y={y - 11} width={width} height={22} rx={11} />
      <text x={centre} y={y + 5}>{text}</text>
    </g>
  )
}

/**
 * One alternative, framed on its own rows across the full page width.
 *
 * The heading names the branch and its rank; the lifeline strip under it
 * repeats the participants so the section reads as a self-contained ladder;
 * the separator above it says the section EXCLUDES the one before, which the
 * vertical stacking alone would not.
 */
const BranchSection = (
  { block, branch, lifelines, padding }: {
    block: Block
    branch: BlockBranch
    lifelines: readonly Lifeline[]
    padding: number
  }
): JSX.Element => (
  <g className="fv-block-branch">
    {branch.separatorY !== undefined && (
      <BandMark
        x0={block.x0}
        x1={block.x1}
        y={branch.separatorY}
        text="— OR —"
        className="fv-alt-separator"
      />
    )}
    <rect
      x={branch.x0 - padding}
      y={branch.y0}
      width={branch.x1 - branch.x0 + padding * 2}
      height={branch.y1 - branch.y0}
      rx={6}
    />
    <text x={branch.x0 - padding + 8} y={branch.y0 + 15} className="fv-branch-label">
      {`alt ${branch.index + 1}/${block.branches.length} — ${branch.name}`}
    </text>
    {/* The stack is not a timeline: every section's clock starts at the alt
        entry, and each arrow's own badge names the step its delay is from. */}
    <text x={branch.x1 + padding - 8} y={branch.y0 + 15} className="fv-branch-note">
      {`${branch.rowCount} step${branch.rowCount === 1 ? "" : "s"} · time restarts at the alt entry`}
    </text>
    {lifelines.map((lifeline) => (
      <text key={lifeline.id} x={lifeline.x} y={branch.headingY + 12} className="fv-branch-lifeline">
        {lifeline.label}
      </text>
    ))}
  </g>
)

const BlockShape = (
  { block, lifelines, padding }: {
    block: Block
    lifelines: readonly Lifeline[]
    padding: number
  }
): JSX.Element => (
  <g className={`fv-block fv-block-${block.kind}`}>
    <rect x={block.x0 - 30} y={block.y0} width={block.x1 - block.x0 + 60} height={block.y1 - block.y0} rx={8} />
    <text x={block.x0 - 24} y={block.y0 + 14} className="fv-block-label">
      {block.kind === "alt"
        ? `alt ${block.id} — ${block.branches.length} exclusive alternatives, exactly one of them runs`
        : `unordered ${block.id}`}
    </text>
    {block.branches.map((branch) => (
      <BranchSection
        key={branch.frameId}
        block={block}
        branch={branch}
        lifelines={lifelines}
        padding={padding}
      />
    ))}
    {block.rejoinY !== undefined && (
      <BandMark
        x0={block.x0}
        x1={block.x1}
        y={block.rejoinY}
        text="all alternatives rejoin here"
        className="fv-alt-rejoin"
      />
    )}
  </g>
)

/** The pill a bead is drawn in, sized from its text so the ladder stays tight. */
const beadWidth = (text: string): number => text.length * 6.2 + 12

const Bead = (
  { centre, y, text, className, title }: {
    centre: number
    y: number
    text: string
    className: string
    title: string
  }
): JSX.Element => {
  const width = beadWidth(text)
  return (
    <g className={className}>
      <rect x={centre - width / 2} y={y - 8} width={width} height={16} rx={8} />
      <text x={centre} y={y + 4}>{text}</text>
      <title>{title}</title>
    </g>
  )
}

/** The placeholder holding the to-bead's place while no To-tag is stated. */
const NO_TAG = "to—"

/** `1st`, `2nd`, `3rd`, `4th`… for a title that counts tags. */
const ordinalWord = (n: number): string => {
  const suffix = n % 100 >= 11 && n % 100 <= 13 ? "th" : (["th", "st", "nd", "rd"][n % 10] ?? "th")
  return `${n}${suffix}`
}

const fromTitle = (dialog: DialogTag): string =>
  `${dialog.from.token}: the tag of the party that originated leg ${dialog.legId}'s dialog — constant on every message of the leg` +
  (dialog.from.value === undefined ? "" : ` (${dialog.from.value})`)

const toTitle = (dialog: DialogTag): string => {
  if (dialog.to === undefined) {
    return `${NO_TAG}: no To-tag yet — nobody has answered leg ${dialog.legId}, so no dialog is established`
  }
  if (dialog.to.value !== undefined) {
    return dialog.to.forked
      ? `${dialog.to.token}: answering tag ${dialog.to.value} — the ${ordinalWord(dialog.to.ordinal)} distinct To-tag leg ${dialog.legId} was answered under; a new token is a new tag on the wire`
      : `${dialog.to.token}: the answering tag of leg ${dialog.legId}'s only dialog (${dialog.to.value})`
  }
  if (!dialog.to.forked) return `${dialog.to.token}: the answering tag of leg ${dialog.legId}'s only dialog`
  return dialog.to.earlyId === undefined
    ? `${dialog.to.token}: leg ${dialog.legId} has named no early dialog yet — a tag stated before its fork`
    : `${dialog.to.token}: the answering tag of early dialog "${dialog.to.earlyId}" — one of leg ${dialog.legId}'s forks, and the tag every later message of the dialog it confirmed restates`
}

/**
 * The dialog beads: the derived tag PAIR every message states.
 *
 * Requests and responses of one dialog carry the same pair, so a fork reads as
 * one from-token against several to-tokens, each in the fork's own colour. Each
 * half is drawn at the end of the arrow belonging to the party whose tag it is;
 * where the arrow is too short for two beads they collapse into one pill on the
 * arrow instead of overlapping.
 */
const DialogBeads = ({ event }: { event: MessageEvent }): JSX.Element | null => {
  const dialog = event.dialog
  if (dialog === undefined) return null
  const actorX = event.direction === "toSut" ? event.x1 : event.x2
  const sutX = event.direction === "toSut" ? event.x2 : event.x1
  const inward = sutX >= actorX ? 1 : -1
  const fromText = dialog.from.token
  const toText = dialog.to?.token ?? NO_TAG
  const absent = dialog.to === undefined ? " fv-dialog-absent" : ""
  const forkClass = dialog.to?.forked === true ? ` fv-fork-${dialog.to.ordinal}` : ""

  if (beadWidth(fromText) + beadWidth(toText) + 24 > Math.abs(sutX - actorX)) {
    const width = beadWidth(`${fromText} · ${toText}`)
    const centre = (actorX + sutX) / 2
    return (
      <g className={`fv-dialog fv-dialog-pair${absent}${forkClass}`}>
        <rect x={centre - width / 2} y={event.y - 8} width={width} height={16} rx={8} />
        <text x={centre} y={event.y + 4}>
          <tspan className="fv-dialog-from-tag">{fromText}</tspan>
          <tspan className="fv-dialog-sep">{" · "}</tspan>
          <tspan className="fv-dialog-to-tag">{toText}</tspan>
        </text>
        <title>{`${fromTitle(dialog)}\n${toTitle(dialog)}`}</title>
      </g>
    )
  }

  const centreOf = (owner: "actor" | "sut", text: string): number =>
    owner === "actor"
      ? actorX + inward * (beadWidth(text) / 2 + 6)
      : sutX - inward * (beadWidth(text) / 2 + 6)
  const answerer = dialog.from.owner === "actor" ? "sut" : "actor"
  return (
    <>
      <Bead
        centre={centreOf(dialog.from.owner, fromText)}
        y={event.y}
        text={fromText}
        className="fv-dialog fv-dialog-from"
        title={fromTitle(dialog)}
      />
      <Bead
        centre={centreOf(dialog.to?.owner ?? answerer, toText)}
        y={event.y}
        text={toText}
        className={`fv-dialog fv-dialog-to${absent}${forkClass}`}
        title={toTitle(dialog)}
      />
    </>
  )
}

/**
 * A ruling, banded across the row it was made at and named in the gutter.
 *
 * The band is what makes a failure findable while scrolling a long recording:
 * the step id alone is a coordinate the reader has to already be looking for.
 */
const MarkBand = (
  { event, layout, mark }: { event: MessageEvent; layout: LadderLayout; mark: LadderMark }
): JSX.Element => (
  <g className={`fv-mark fv-mark-${mark.tone}`}>
    <rect x={0} y={event.y - 19} width={layout.width} height={38} className="fv-mark-band" />
    <text x={Math.max(event.x1, event.x2) + 12} y={event.y + 17} className="fv-mark-label">
      {mark.text}
      <title>{mark.title}</title>
    </text>
  </g>
)

const Arrow = (
  { event, layout, selected, status, mark, onSelect }: {
    event: MessageEvent
    layout: LadderLayout
    selected: boolean
    status: EventStatus | undefined
    mark: LadderMark | undefined
    onSelect: ((event: LadderEvent) => void) | undefined
  }
): JSX.Element => {
  const badges = decorationBadges(event.decorations)
  const auto = event.decorations.some((decoration) => decoration.kind === "auto")
  const optional = event.decorations.some((decoration) => decoration.kind === "optional")
  const midX = (event.x1 + event.x2) / 2
  const forked = event.dialog?.forked === true
  const classes = [
    "fv-arrow",
    `fv-arrow-${event.direction}`,
    auto ? "fv-arrow-auto" : "",
    optional ? "fv-arrow-optional" : "",
    forked ? `fv-forked fv-fork-${event.dialog?.ordinal ?? 0}` : "",
    status ? `fv-arrow-${status}` : "",
    mark ? `fv-arrow-marked fv-marked-${mark.tone}` : "",
    selected ? "fv-arrow-selected" : ""
  ].filter(Boolean).join(" ") + callTint(layout, event.callSlot)

  return (
    <g className={classes} onClick={onSelect ? () => onSelect(event) : undefined}>
      {mark && <MarkBand event={event} layout={layout} mark={mark} />}
      <rect
        x={Math.min(event.x1, event.x2) - 4}
        y={event.y - 20}
        width={Math.abs(event.x2 - event.x1) + 8}
        height={38}
        className="fv-arrow-hit"
      />
      <line x1={event.x1} x2={event.x2} y1={event.y} y2={event.y} markerEnd="url(#fv-arrowhead)" />
      <DialogBeads event={event} />
      <text x={midX} y={event.y - 6} className="fv-arrow-label">{event.label}</text>
      <text x={midX} y={event.y + 13} className="fv-arrow-sub">
        {[
          event.id,
          layout.calls.length > 1 ? event.callId : undefined,
          // The beads state the dialog; the sublabel only names its leg.
          event.legId,
          event.sublabel
        ].filter(Boolean).join(" · ")}
      </text>
      {badges.length > 0 && (
        <text x={Math.max(event.x1, event.x2) + 12} y={event.y + 4} className="fv-arrow-badges">
          {badges.map((badge) => badge.text).join("  ")}
          <title>{badges.map((badge) => `${badge.text}: ${badge.title}`).join("\n")}</title>
        </text>
      )}
    </g>
  )
}

const InjectMark = (
  { event, layout, selected, onSelect }: {
    event: Extract<LadderEvent, { kind: "inject" }>
    layout: LadderLayout
    selected: boolean
    onSelect: ((event: LadderEvent) => void) | undefined
  }
): JSX.Element => {
  const first = layout.lifelines[0]?.x ?? 0
  const last = layout.lifelines[layout.lifelines.length - 1]?.x ?? 0
  return (
    <g
      className={`fv-inject${selected ? " fv-inject-selected" : ""}`}
      onClick={onSelect ? () => onSelect(event) : undefined}
    >
      <line x1={first - 40} x2={last + 40} y1={event.y} y2={event.y} />
      <rect x={(first + last) / 2 - 130} y={event.y - 13} width={260} height={26} rx={13} />
      <text x={(first + last) / 2} y={event.y + 5} className="fv-inject-label">
        {`inject ${event.action}${event.target ? ` -> ${event.target}` : ""}`}
      </text>
    </g>
  )
}

export const Ladder = (
  { layout, selectedId, onSelect, statusById, marksById }: LadderProps
): JSX.Element => (
  <svg
    className="fv-ladder"
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
    {layout.lifelines.map((lifeline) => (
      <LifelineHead
        key={lifeline.id}
        lifeline={lifeline}
        height={layout.height}
        tint={callTint(layout, lifeline.callSlot)}
      />
    ))}
    {/* Over the lifelines, under the arrows: a section's frame and its repeated
        participant strip must read on top of the lines they band. */}
    {layout.blocks.map((block) => (
      <BlockShape
        key={block.id}
        block={block}
        lifelines={layout.lifelines}
        padding={layout.options.sectionPadding}
      />
    ))}
    {layout.events.map((event) =>
      isMessageEvent(event)
        ? (
          <Arrow
            key={event.id}
            event={event}
            layout={layout}
            selected={event.id === selectedId}
            status={statusById?.[event.id]}
            mark={marksById?.[event.id]}
            onSelect={onSelect}
          />
        )
        : (
          <InjectMark
            key={event.id}
            event={event}
            layout={layout}
            selected={event.id === selectedId}
            onSelect={onSelect}
          />
        )
    )}
  </svg>
)
