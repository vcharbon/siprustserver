/** A value too long to show, as a named chip: the value is its tooltip, a click copies it. */
import type { JSX } from "react"
import { useCopy } from "../use-copy.ts"

export const CopyChip = ({ label, value }: { label: string; value: string }): JSX.Element => {
  const copy = useCopy()
  return (
    <button
      type="button"
      className="copy-chip"
      title={`${value}\nclick: copy`}
      onClick={() => copy.copy(value)}
    >
      {copy.label(label)}
    </button>
  )
}
