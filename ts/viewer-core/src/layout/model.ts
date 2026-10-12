/**
 * The ladder layout model: the pure geometric result of laying out a flow.
 *
 * Nothing here knows about React or the DOM. A renderer consumes this model
 * and draws it; a test asserts against it directly.
 */
import type { Inject } from "../generated/pivot-v3.ts"
import type { CallBand } from "./calls.ts"
import type { DialogTag, DialogToken } from "./dialogs.ts"

/** The synthetic lifeline every peer actor exchanges messages with. */
export const SUT_LIFELINE = "__sut__"

/** The frame every node outside an `alt` branch is laid out in. */
export const ROOT_FRAME = "root"

export interface LayoutOptions {
  /** Horizontal distance between two adjacent lifelines in the root frame. */
  readonly laneWidth: number
  /** Vertical distance between two adjacent event rows. */
  readonly rowHeight: number
  /** Left and right page margin. */
  readonly marginX: number
  /** Space above the first event row, holding the lifeline heads. */
  readonly headerHeight: number
  /** Space below the last event row. */
  readonly footerHeight: number
  /** Horizontal inset of an `alt` branch section box beyond the lifeline span. */
  readonly sectionPadding: number
  /** Right-hand room for the decoration badges of the last lifeline's arrows. */
  readonly badgeGutter: number
}

export const defaultLayoutOptions: LayoutOptions = {
  laneWidth: 220,
  rowHeight: 46,
  marginX: 90,
  headerHeight: 96,
  footerHeight: 40,
  sectionPadding: 30,
  badgeGutter: 280
}

export type LifelineKind = "actor" | "sut"

export interface Lifeline {
  readonly id: string
  readonly label: string
  readonly sublabel: string | undefined
  readonly kind: LifelineKind
  /** Actor kind (`uac`, `uas`, `mrf`) where the lifeline is an actor. */
  readonly actorKind: string | undefined
  readonly legs: readonly string[]
  /** Background policies the actor answers outside the flow. */
  readonly background: readonly string[]
  /** Calls this actor's legs belong to, in `calls[]` order. */
  readonly callIds: readonly string[]
  /** The colour slot when every leg of the actor belongs to ONE call. */
  readonly callSlot: number | undefined
  readonly index: number
  readonly x: number
}

/**
 * A band the ladder is drawn in. Every frame spans the page on the SAME
 * lifeline positions: an `alt` branch is a full-width section stacked under its
 * predecessor, so a reader follows one alternative end to end at normal arrow
 * size instead of reading three narrow lanes at once.
 */
export interface Frame {
  readonly id: string
  readonly altId: string | undefined
  readonly branchName: string | undefined
  /** Position among its alt's branches; 0 for the root frame. */
  readonly branchIndex: number
  readonly x0: number
  readonly x1: number
  readonly lifelineX: Readonly<Record<string, number>>
}

export type Decoration =
  | { readonly kind: "delay"; readonly ms: number; readonly from: string; readonly compressible: boolean; readonly timerLinked: boolean }
  | { readonly kind: "auto" }
  | { readonly kind: "optional" }
  | { readonly kind: "retransmits"; readonly count: number }
  | { readonly kind: "deviation"; readonly deviationId: string; readonly deviationKind: string }
  | { readonly kind: "after"; readonly refs: readonly string[] }
  | { readonly kind: "check"; readonly mode: "assert" | "record" }
  | { readonly kind: "checks"; readonly count: number }
  | { readonly kind: "within"; readonly ms: number }
  | { readonly kind: "overlap"; readonly with: string }
  | { readonly kind: "early"; readonly id: string }
  | { readonly kind: "body"; readonly summary: string }
  | { readonly kind: "observed"; readonly atUs: number }
  | { readonly kind: "defect"; readonly description: string | undefined }
  | { readonly kind: "step"; readonly id: string }

export type EventOrigin = "scenario" | "recording"

export interface MessageEvent {
  readonly kind: "message"
  readonly id: string
  readonly row: number
  readonly y: number
  readonly frameId: string
  readonly legId: string
  readonly actorId: string
  readonly op: "send" | "expect"
  /** `toSut` leaves the actor lifeline, `fromSut` arrives at it. */
  readonly direction: "toSut" | "fromSut"
  readonly fromLifeline: string
  readonly toLifeline: string
  readonly x1: number
  readonly x2: number
  readonly label: string
  readonly sublabel: string | undefined
  readonly decorations: readonly Decoration[]
  readonly altId: string | undefined
  readonly branchName: string | undefined
  readonly groupId: string | undefined
  /** The dialog this message rides, as a derived token pair — never real tags. */
  readonly dialog: DialogTag | undefined
  readonly callId: string | undefined
  readonly callSlot: number | undefined
  readonly origin: EventOrigin
  /** The document step, verbatim, for the raw-JSON side panel. */
  readonly source: unknown
}

export interface InjectEvent {
  readonly kind: "inject"
  readonly id: string
  readonly row: number
  readonly y: number
  readonly frameId: string
  readonly action: string
  readonly target: string | undefined
  readonly decorations: readonly Decoration[]
  readonly origin: EventOrigin
  readonly source: Inject
}

export type LadderEvent = MessageEvent | InjectEvent

/**
 * One `alt` branch as a full-width section of the ladder. Sections stack in
 * declaration order and each states its own heading; `separatorY`, present on
 * every section but the first, carries the exclusive-or mark that stops the
 * stack from being read as a sequence.
 */
export interface BlockBranch {
  readonly name: string
  readonly frameId: string
  readonly index: number
  readonly x0: number
  readonly x1: number
  readonly rowCount: number
  /** The heading row; the branch's own steps start on the next one. */
  readonly rowStart: number
  /** Exclusive. */
  readonly rowEnd: number
  readonly y0: number
  readonly y1: number
  /** Centre of the heading band, holding the label and the lifeline strip. */
  readonly headingY: number
  /** Centre of the "or" separator row above this section; absent on the first. */
  readonly separatorY: number | undefined
}

export interface Block {
  readonly kind: "alt" | "unordered"
  readonly id: string
  readonly rowStart: number
  /** Exclusive. */
  readonly rowEnd: number
  readonly y0: number
  readonly y1: number
  readonly x0: number
  readonly x1: number
  readonly branches: readonly BlockBranch[]
  /** Where the alternatives converge and the shared flow resumes; `alt` only. */
  readonly rejoinY: number | undefined
}

export interface LadderLayout {
  readonly title: string
  readonly subtitle: string | undefined
  readonly lifelines: readonly Lifeline[]
  readonly frames: Readonly<Record<string, Frame>>
  readonly events: readonly LadderEvent[]
  readonly blocks: readonly Block[]
  /** The document's calls, in declaration order: the colour legend reads these. */
  readonly calls: readonly CallBand[]
  /** Every derived dialog token, for the legend and the fork colouring. */
  readonly dialogs: readonly DialogToken[]
  readonly rowCount: number
  readonly width: number
  readonly height: number
  readonly options: LayoutOptions
  readonly warnings: readonly string[]
}

/** Narrowing helper: the model is consumed by renderers that switch on kind. */
export const isMessageEvent = (event: LadderEvent): event is MessageEvent => event.kind === "message"
