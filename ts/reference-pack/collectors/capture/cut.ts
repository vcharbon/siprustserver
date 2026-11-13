/**
 * The `capture` collector's cut: the packets of a pcapng capture a cell owns,
 * as a capture of their own. The capture is the B2BUA runner's
 * (`B2BUA_CAPTURE`): little-endian sections, each one raw-IP interface, one
 * enhanced packet block per UDP datagram. A cell owns each datagram whose
 * Call-ID is one of its calls'. The cut is the first section's header and
 * interface, then the owned packets in capture order.
 */

const SECTION_HEADER = 0x0a0d0d0a
const INTERFACE = 1
const ENHANCED_PACKET = 6
const BYTE_ORDER_MAGIC = 0x1a2b3c4d
const LINKTYPE_RAW = 101
const UDP_HEADER = 8

/** One block of a capture: its type and its bytes, its type and lengths included. */
export interface Block {
  readonly type: number
  readonly bytes: Uint8Array
}

/** The blocks of `capture`, in order; a section another writer made, or a block cut short, is refused. */
export const blocksOf = (capture: Uint8Array): ReadonlyArray<Block> => {
  const view = new DataView(capture.buffer, capture.byteOffset, capture.byteLength)
  const blocks: Array<Block> = []
  for (let at = 0; at + 12 <= capture.byteLength;) {
    const type = view.getUint32(at, true)
    const length = view.getUint32(at + 4, true)
    if (length < 12 || at + length > capture.byteLength) throw new Error(`a block cut short at byte ${at}`)
    if (type === SECTION_HEADER && view.getUint32(at + 8, true) !== BYTE_ORDER_MAGIC) {
      throw new Error(`a section at byte ${at} is not little-endian, which the runner writes`)
    }
    if (type === INTERFACE && view.getUint16(at + 8, true) !== LINKTYPE_RAW) {
      throw new Error(`an interface at byte ${at} is not raw IP, which the runner writes`)
    }
    blocks.push({ type, bytes: capture.subarray(at, at + length) })
    at += length
  }
  return blocks
}

/** The UDP payload of an enhanced packet block's raw-IP packet; empty where it holds none. */
export const payloadOf = (block: Block): Uint8Array => {
  const view = new DataView(block.bytes.buffer, block.bytes.byteOffset, block.bytes.byteLength)
  const captured = view.getUint32(20, true)
  const packet = block.bytes.subarray(28, 28 + captured)
  const version = packet[0] === undefined ? 0 : packet[0] >> 4
  const header = version === 4 ? (packet[0]! & 0x0f) * 4 : version === 6 ? 40 : undefined
  return header === undefined ? new Uint8Array() : packet.subarray(header + UDP_HEADER)
}

const CALL_ID = /^(?:call-id|i)[ \t]*:[ \t]*(\S+)/im

/** The Call-ID of a SIP message's bytes, where it has one. */
export const callIdOf = (message: Uint8Array): string | undefined =>
  CALL_ID.exec(new TextDecoder("latin1").decode(message))?.[1]

/** The start line of a SIP message's bytes, the request line or the status line. */
export const startLineOf = (message: Uint8Array): string => new TextDecoder("latin1").decode(message).split("\r\n", 1)[0] ?? ""

/**
 * The `table` attachment's text of the packets `capture` holds: its columns,
 * then a row per packet, its number, its SIP message's start line and its
 * Call-ID.
 */
export const packetTableOf = (capture: Uint8Array): string => {
  const packets = blocksOf(capture).filter((block) => block.type === ENHANCED_PACKET).map(payloadOf)
  const rows = packets.map((message, at) => [String(at + 1), startLineOf(message), callIdOf(message) ?? ""])
  return [["packet", "start line", "Call-ID"], ...rows].map((row) => `${JSON.stringify(row)}\n`).join("")
}

/** The capture of `blocks` a cell owning `callIds` owns: the first section's header and interface, then its packets. */
export const cut = (blocks: ReadonlyArray<Block>, callIds: ReadonlyArray<string>): Uint8Array => {
  const owned = new Set(callIds)
  const header = [blocks.find((block) => block.type === SECTION_HEADER), blocks.find((block) => block.type === INTERFACE)]
  const packets = blocks.filter((block) => {
    if (block.type !== ENHANCED_PACKET) return false
    const callId = callIdOf(payloadOf(block))
    return callId !== undefined && owned.has(callId)
  })
  const parts = [...header.flatMap((block) => (block === undefined ? [] : [block.bytes])), ...packets.map((block) => block.bytes)]
  const out = new Uint8Array(parts.reduce((total, part) => total + part.byteLength, 0))
  parts.reduce((at, part) => (out.set(part, at), at + part.byteLength), 0)
  return out
}
