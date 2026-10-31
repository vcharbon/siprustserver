/** A value too long to show, as a named chip: the value is its tooltip, a click copies it. */
import type { JSX } from "react"
import { useState } from "react"

export const CopyChip = ({ label, value }: { label: string; value: string }): JSX.Element => {
  const [copied, setCopied] = useState(false)
  return (
    <button
      type="button"
      className="copy-chip"
      title={`${value}\nclick: copy`}
      onClick={() => {
        void navigator.clipboard?.writeText(value).then(() => {
          setCopied(true)
          window.setTimeout(() => setCopied(false), 1200)
        })
      }}
    >
      {copied ? "copied" : label}
    </button>
  )
}
