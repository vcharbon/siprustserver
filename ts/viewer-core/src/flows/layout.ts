/**
 * The source ladder: every captured message, drawn between the sockets that
 * exchanged it.
 *
 * Deliberately uninterpreted. No actors, no legs-as-roles, no steps: one row
 * per datagram, in capture order, so a confusing generated case can be read
 * against the bytes it came from. It is a TRIAGE view, so the whole capture is
 * drawn by default and the legs a case selected are told apart by colour, never
 * by hiding the rest.
 */
import { wireDialogs, type WireDialog, type WireDialogs } from "./dialogs.ts"
import { callIdFamilies, type CallIdFamilies, shortCallId } from "./families.ts"
import { flowsMessageLabel, type FlowsDocument, type FlowsMessage } from "./parse.ts"
import { type CaptureSelection, messageCoordinate } from "./selection.ts"

export interface FlowsLayoutOptions {
  readonly laneWidth: number
  readonly rowHeight: number
  readonly marginX: number
  readonly headerHeight: number
  readonly footerHeight: number
  readonly rightGutter: number
  readonly legChipWidth: number
  readonly legChipHeight: number
  readonly legChipGap: number
}

export const defaultFlowsLayoutOptions: FlowsLayoutOptions = {
  laneWidth: 340,
  rowHeight: 38,
  marginX: 110,
  headerHeight: 64,
  footerHeight: 30,
  rightGutter: 140,
  legChipWidth: 318,
  legChipHeight: 24,
  legChipGap: 6
}

export interface SocketLifeline {
  readonly id: string
  readonly index: number
  readonly x: number
  /** The case's own steps put the SUT at this socket. */
  readonly sut: boolean
}

/**
 * One socket the capture used, whether or not this layout draws it.
 *
 * The roster is computed before the socket filter so the filter's own control
 * can offer every address; a chip that disappears when clicked cannot be
 * clicked again.
 */
export interface CaptureSocket {
  readonly id: string
  /** Messages this socket sent or received, before the socket filter. */
  readonly messageCount: number
  /** The case's steps name it as the SUT's. */
  readonly sut: boolean
  /** This layout draws it. */
  readonly drawn: boolean
}

/** A leg of the capture, as its header chip and its legend line state it. */
export interface FlowsLegHeader {
  readonly legIndex: number
  readonly callId: string
  readonly shortCallId: string
  readonly familyIndex: number
  readonly hueSlot: number
  readonly shade: number
  /** The case cites at least one message of this leg. */
  readonly selected: boolean
  /**
   * Why an UNSELECTED leg sits in one of the case's call groups, off the
   * group's own correlation evidence. Undefined on a selected leg and on a
   * leg outside the case's groups.
   */
  readonly rideAlong: string | undefined
  readonly derivedFrom: number | undefined
  readonly messageCount: number
  /** Milliseconds from the capture's first drawn message. */
  readonly firstAtMs: number
  readonly finalStatus: number | undefined
  /** This layout draws it: a leg filter can leave it out. */
  readonly drawn: boolean
  readonly x: number
  readonly y: number
  readonly width: number
  readonly height: number
}

export interface FlowsRow {
  /** Row index, and the React key: the capture coordinate is unique. */
  readonly id: string
  readonly row: number
  readonly y: number
  readonly x1: number
  readonly x2: number
  readonly label: string
  readonly sublabel: string
  /** Milliseconds from the first drawn message. */
  readonly atMs: number
  readonly retx: boolean
  readonly legIndex: number
  readonly callId: string
  readonly hueSlot: number
  readonly shade: number
  /** The case cites this leg. */
  readonly selected: boolean
  /** The step citing this exact message, where one does. */
  readonly stepId: string | undefined
  /** The real tag pair the datagram states, named and coloured. */
  readonly dialog: WireDialog | undefined
  readonly message: FlowsMessage
}

export interface FlowsLayout {
  readonly title: string
  readonly subtitle: string
  readonly lifelines: readonly SocketLifeline[]
  /** Every socket of the capture, drawn or filtered out. */
  readonly sockets: readonly CaptureSocket[]
  /** The SUT's own sockets, as the case's steps name them; empty where it cites none. */
  readonly sutSockets: readonly string[]
  readonly legs: readonly FlowsLegHeader[]
  readonly rows: readonly FlowsRow[]
  readonly families: CallIdFamilies
  /** The wire tags every drawn leg states, and the forks among them. */
  readonly dialogs: WireDialogs
  /** True where the case cited coordinates, so pale means DISCARDED. */
  readonly hasSelection: boolean
  /** Messages the document carries that this layout does not draw. */
  readonly hiddenMessages: number
  /** Top of the socket boxes: the leg band sits above them. */
  readonly socketBoxY: number
  readonly lifelineTop: number
  readonly width: number
  readonly height: number
  readonly options: FlowsLayoutOptions
  readonly warnings: readonly string[]
}

export interface FlowsLayoutInput {
  /** Call groups a case was cut from, as its `source.call_groups` states them. */
  readonly groups?: readonly number[] | undefined
  /** Draw only those groups. Default false: triage reads the WHOLE capture. */
  readonly restrictToGroups?: boolean | undefined
  /**
   * Draw only the legs the case SELECTED. Default false, which is the triage
   * reading; a capture whose discarded legs bury the cut one is why this
   * exists. Without a selection it holds nothing back.
   */
  readonly onlySelected?: boolean | undefined
  /**
   * Draw only messages one of these sockets sent or received. Empty or absent
   * draws every socket, which is the triage reading.
   */
  readonly sockets?: readonly string[] | undefined
  /**
   * Draw only these legs. Empty or absent draws every one of them. It is the
   * reader's own narrowing, unrelated to what the case selected: a capture
   * holding one confusing leg is read by putting the others away for a moment.
   */
  readonly legs?: readonly number[] | undefined
  /** The case's capture coordinates; without them every leg reads as selected. */
  readonly selection?: CaptureSelection | undefined
  /**
   * The SUT as the cut stated it, `ip` or `ip:port` each: what orients a ladder
   * whose selection carries no steps to read the SUT off — a refused case.
   */
  readonly sut?: readonly string[] | undefined
  readonly title?: string | undefined
}

/** The address of a socket: `ip:port` without its port, an `ip` as it is — as the cut reads one. */
const addressOf = (socket: string): string => {
  const colon = socket.lastIndexOf(":")
  return colon === -1 ? socket : socket.slice(0, colon)
}

/**
 * The SUT's sockets: read off the case's own steps, else the drawn sockets at
 * the addresses the cut stated.
 *
 * A `send` step is the ACTOR emitting, so the captured datagram's destination
 * is the SUT; an `expect` step is the SUT emitting, so its source is. Nothing
 * else in a capture says which address is under test — the flows document is
 * uninterpreted — so a case citing no coordinate and a cut stating no address
 * leave this empty rather than guessing from message counts.
 */
const sutSocketsOf = (
  document: FlowsDocument,
  selection: CaptureSelection | undefined,
  stated: readonly string[]
): readonly string[] => {
  const counts = new Map<string, number>()
  if (selection !== undefined && !selection.empty) {
    for (const message of document.messages) {
      const op = selection.opByMessage.get(messageCoordinate(message.legIndex, message.msgIndex))
      if (op === undefined) continue
      const socket = op === "send" ? message.dst : message.src
      counts.set(socket, (counts.get(socket) ?? 0) + 1)
    }
  }
  if (counts.size === 0 && stated.length > 0) {
    const addresses = new Set(stated)
    for (const message of document.messages) {
      for (const socket of [message.src, message.dst]) {
        if (!addresses.has(socket) && !addresses.has(addressOf(socket))) continue
        counts.set(socket, (counts.get(socket) ?? 0) + 1)
      }
    }
  }
  return [...counts.entries()].sort(([, left], [, right]) => right - left).map(([socket]) => socket)
}

const legSelection = (
  document: FlowsDocument,
  groups: readonly number[] | undefined,
  warnings: string[]
): ReadonlySet<number> | undefined => {
  if (groups === undefined || groups.length === 0) return undefined
  const selected = new Set<number>()
  for (const index of groups) {
    const group = document.groups[index]
    if (group === undefined) {
      warnings.push(`call group ${index} is not in this flows document; drawing every leg`)
      return undefined
    }
    for (const leg of group.legs) selected.add(leg)
  }
  return selected.size === 0 ? undefined : selected
}

/** Lays a flows document out: one row per message, sockets as lifelines. */
export const buildFlowsLayout = (
  document: FlowsDocument,
  input: FlowsLayoutInput = {},
  overrides: Partial<FlowsLayoutOptions> = {}
): FlowsLayout => {
  const options = { ...defaultFlowsLayoutOptions, ...overrides }
  const warnings = [...document.warnings]
  const families = callIdFamilies(document)

  const selection = input.selection
  const hasSelection = selection !== undefined && !selection.empty
  const isSelected = (legIndex: number): boolean => !hasSelection || selection.legs.has(legIndex)
  // A family warning is about the legs it names: a ladder of the selected
  // legs alone says nothing of the legs it does not draw.
  const onlySelected = input.onlySelected === true && hasSelection
  warnings.push(
    ...families.warnings
      .filter((warning) => !onlySelected || warning.legs.some(isSelected))
      .map((warning) => warning.text)
  )

  // A leg the case cites that this READ did not bring back is drawn nowhere and
  // counted nowhere: the ladder says so FIRST — a pane shows six warnings, and
  // nothing it says outranks the legs the reader opened the case for.
  const outside = !hasSelection ? [] : [...selection.legs]
    .filter((leg) => leg >= 0 && leg < document.legs.length && document.legs[leg] === undefined)
    .sort((left, right) => left - right)
  if (outside.length > 0) {
    const named = outside.slice(0, 6).join(", ")
    warnings.unshift(
      `${outside.length} legs of this case lie outside the window drawn: ${named}` +
        (outside.length > 6 ? ", …" : "")
    )
  }

  const drawn = input.restrictToGroups === true ? legSelection(document, input.groups, warnings) : undefined
  const drawable = document.messages.filter((message) =>
    (drawn === undefined || drawn.has(message.legIndex)) &&
    (!onlySelected || isSelected(message.legIndex))
  )

  const sutSockets = sutSocketsOf(document, selection, input.sut ?? [])
  const socketFilter = new Set(input.sockets ?? [])
  const legFilter = new Set(input.legs ?? [])
  const touches = (message: FlowsMessage): boolean =>
    socketFilter.size === 0 || socketFilter.has(message.src) || socketFilter.has(message.dst)
  const kept = (message: FlowsMessage): boolean =>
    legFilter.size === 0 || legFilter.has(message.legIndex)
  const messages = drawable.filter((message) => touches(message) && kept(message))
  const hidden = document.messages.length - messages.length

  // Both rosters count over what the CASE's filters left, never over the
  // reader's own: a chip must state what clicking it would draw, and a count
  // that falls to zero as its own filter narrows cannot be clicked back.
  const socketCounts = new Map<string, number>()
  for (const message of drawable) {
    for (const socket of new Set([message.src, message.dst])) {
      socketCounts.set(socket, (socketCounts.get(socket) ?? 0) + 1)
    }
  }
  const shown = new Set(messages.flatMap((message) => [message.src, message.dst]))
  const sockets: CaptureSocket[] = [...socketCounts.keys()].sort().map((id) => ({
    id,
    messageCount: socketCounts.get(id) ?? 0,
    sut: sutSockets.includes(id),
    drawn: shown.has(id)
  }))
  const lifelines: SocketLifeline[] = sockets
    .filter((socket) => socket.drawn)
    .map((socket, index) => ({
      id: socket.id,
      index,
      x: options.marginX + index * options.laneWidth,
      sut: socket.sut
    }))
  const xOf = new Map(lifelines.map((lifeline) => [lifeline.id, lifeline.x]))
  const t0 = messages[0]?.tsUs ?? 0
  const last = lifelines[lifelines.length - 1]
  const width = (last?.x ?? options.marginX) + options.rightGutter

  const bandLegs = [...new Set(drawable.map((message) => message.legIndex))].sort((a, b) => a - b)
  const shownLegs = new Set(messages.map((message) => message.legIndex))
  const perRow = Math.max(1, Math.floor((width - 16) / (options.legChipWidth + options.legChipGap)))
  const bandRows = Math.ceil(bandLegs.length / perRow)
  const bandHeight = bandLegs.length === 0 ? 0 : bandRows * (options.legChipHeight + options.legChipGap) + 8

  const rideAlongOf = (legIndex: number): string | undefined => {
    if (!hasSelection || isSelected(legIndex)) return undefined
    for (const index of input.groups ?? []) {
      const group = document.groups[index]
      if (group === undefined || !group.legs.includes(legIndex)) continue
      const evidence = group.evidence.find((e) => e.legs.includes(legIndex))
      const by = evidence === undefined
        ? "call-group correlation"
        : evidence.header !== undefined
        ? `${evidence.header}${evidence.param === undefined ? "" : ` ${evidence.param}`} match`
        : evidence.kind.replaceAll("_", "-")
      return `in the case's group ${index} by ${by}; not part of the cut`
    }
    return undefined
  }

  const legs: FlowsLegHeader[] = bandLegs.map((legIndex, position) => {
    const leg = document.legs[legIndex]
    const family = families.byLeg.get(legIndex)
    const legMessages = drawable.filter((message) => message.legIndex === legIndex)
    return {
      legIndex,
      callId: leg?.callId ?? `leg ${legIndex}`,
      shortCallId: shortCallId(leg?.callId ?? `leg ${legIndex}`),
      familyIndex: family?.familyIndex ?? 0,
      hueSlot: family?.hueSlot ?? 0,
      shade: family?.shade ?? 0,
      selected: isSelected(legIndex),
      rideAlong: rideAlongOf(legIndex),
      derivedFrom: family?.derivedFrom,
      messageCount: legMessages.length,
      firstAtMs: ((legMessages[0]?.tsUs ?? t0) - t0) / 1000,
      finalStatus: leg?.finalStatus,
      drawn: shownLegs.has(legIndex),
      x: 8 + (position % perRow) * (options.legChipWidth + options.legChipGap),
      y: 8 + Math.floor(position / perRow) * (options.legChipHeight + options.legChipGap),
      width: options.legChipWidth,
      height: options.legChipHeight
    }
  })

  const dialogs = wireDialogs(document, messages)

  const top = bandHeight + options.headerHeight
  const rows: FlowsRow[] = messages.map((message, row) => {
    const family = families.byLeg.get(message.legIndex)
    const id = messageCoordinate(message.legIndex, message.msgIndex)
    const dialog = dialogs.byMessage.get(id)
    return {
      id,
      row,
      y: top + row * options.rowHeight + options.rowHeight / 2,
      x1: xOf.get(message.src) ?? options.marginX,
      x2: xOf.get(message.dst) ?? options.marginX,
      label: flowsMessageLabel(message),
      // The Call-ID gives way to the tag beads on a row that draws them: the
      // beads identify the dialog, the leg chip already states the family, and
      // a truncated Call-ID between two beads is what makes the row unreadable.
      sublabel: [
        `leg ${message.legIndex}/${message.msgIndex}`,
        dialog === undefined ? shortCallId(message.callId, 20) : undefined,
        message.cseqSeq === undefined ? undefined : `CSeq ${message.cseqSeq} ${message.cseqMethod ?? ""}`.trim(),
        message.retx ? "retransmission" : undefined
      ].filter(Boolean).join(" · "),
      atMs: (message.tsUs - t0) / 1000,
      retx: message.retx,
      legIndex: message.legIndex,
      callId: message.callId,
      hueSlot: family?.hueSlot ?? 0,
      shade: family?.shade ?? 0,
      selected: isSelected(message.legIndex),
      stepId: selection?.stepByMessage.get(id),
      dialog,
      message
    }
  })

  const selectedLegs = legs.filter((leg) => leg.selected).length
  return {
    title: input.title ?? "capture source",
    subtitle: [
      `${messages.length} messages`,
      `${legs.length} legs`,
      `${families.families.length} call-id families`,
      hasSelection ? `${selectedLegs} legs selected by this case` : "no capture coordinates: every leg drawn as selected",
      dialogs.forkedLegs.length === 0
        ? undefined
        : `${dialogs.forkedLegs.length} forked leg(s): ${dialogs.forkedLegs.join(", ")}`,
      sutSockets.length === 0
        ? "no SUT socket: this case cites no capture coordinate and the cut states no address"
        : `SUT at ${sutSockets.join(", ")}`,
      socketFilter.size === 0 ? undefined : `filtered to ${[...socketFilter].join(", ")}`,
      legFilter.size === 0 ? undefined : `filtered to leg ${[...legFilter].sort((a, b) => a - b).join(", ")}`,
      hidden === 0 ? undefined : `${hidden} messages hidden`,
      `schema ${document.schema}`
    ].filter((part) => part !== undefined).join(" · "),
    lifelines,
    sockets,
    sutSockets,
    legs,
    rows,
    families,
    dialogs,
    hasSelection,
    hiddenMessages: hidden,
    socketBoxY: bandHeight + 10,
    lifelineTop: bandHeight + 44,
    width,
    height: top + rows.length * options.rowHeight + options.footerHeight,
    options,
    warnings
  }
}
