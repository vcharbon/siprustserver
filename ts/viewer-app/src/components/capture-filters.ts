/**
 * The capture pane's three narrowings, held as one piece of state.
 *
 * A leg narrows the drawing to one call, a socket to what one address
 * exchanged, a tag lights the messages of one dialog. All three are
 * per-capture, so every pane drawing a capture owns its own — a leg index that
 * means one call in this capture means another in the next.
 */
import { useState } from "react"

export interface CaptureFilters {
  /** Legs the drawing is narrowed to; empty draws every one of them. */
  readonly legs: {
    readonly value: readonly number[]
    readonly onToggle: (legIndex: number) => void
    readonly onClear: () => void
  }
  /** Sockets the drawing is narrowed to; empty draws every one of them. */
  readonly sockets: {
    readonly value: readonly string[]
    readonly onToggle: (socket: string) => void
    readonly onClear: () => void
  }
  /** The dialog tag the reader picked; the rows not stating it fade. */
  readonly tag: {
    readonly value: string | undefined
    readonly onSelect: (tag: string) => void
    readonly onClear: () => void
  }
}

export const useCaptureFilters = (): CaptureFilters => {
  const [legs, setLegs] = useState<readonly number[]>([])
  const [sockets, setSockets] = useState<readonly string[]>([])
  const [tag, setTag] = useState<string | undefined>(undefined)
  return {
    legs: {
      value: legs,
      onToggle: (legIndex) =>
        setLegs((current) =>
          current.includes(legIndex)
            ? current.filter((entry) => entry !== legIndex)
            : [...current, legIndex]
        ),
      onClear: () => setLegs([])
    },
    sockets: {
      value: sockets,
      onToggle: (socket) =>
        setSockets((current) =>
          current.includes(socket)
            ? current.filter((entry) => entry !== socket)
            : [...current, socket]
        ),
      onClear: () => setSockets([])
    },
    tag: {
      value: tag,
      onSelect: setTag,
      onClear: () => setTag(undefined)
    }
  }
}
