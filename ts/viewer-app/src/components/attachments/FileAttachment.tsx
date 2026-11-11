/** A `file` attachment: its size and its download. */
import type { JSX } from "react"
import { formatBytes } from "../../json-size.ts"

export const FileAttachment = ({ bytes, href, name }: {
  readonly bytes: number
  readonly href: string
  readonly name: string
}): JSX.Element => (
  <p>
    <a href={href} download={name}>{name} ↓</a> <span className="muted">{formatBytes(bytes)}</span>
  </p>
)
