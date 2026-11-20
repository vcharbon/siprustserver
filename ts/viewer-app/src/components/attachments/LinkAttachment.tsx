/** A `link` attachment: its URL, opened apart. */
import type { JSX } from "react"

export const LinkAttachment = ({ url }: { readonly url: string }): JSX.Element => (
  <p>
    <a href={url} target="_blank" rel="noreferrer" className="mono">{url} ↗</a>
  </p>
)
