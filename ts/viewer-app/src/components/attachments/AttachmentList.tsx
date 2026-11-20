/**
 * What a holder attached, in the order the kinds are listed (`@sip/contracts`
 * `KINDS`), the holder's order within a kind: each folded under its title and
 * kind, its file a click away, read only once unfolded.
 */
import { KINDS } from "@sip/contracts/attachment-kinds"
import type { JSX } from "react"
import { useState } from "react"
import { type AttachmentListed, attachmentFileHref } from "../../api.ts"
import { AttachmentBody } from "./AttachmentBody.tsx"

/** `listed` in the order of the kinds (`@sip/contracts` `KINDS`), the holder's order kept within a kind. */
export const ordered = (listed: readonly AttachmentListed[]): readonly AttachmentListed[] =>
  KINDS.flatMap((kind) => listed.filter((one) => one.kind === kind))

const Folded = ({ one }: { readonly one: AttachmentListed }): JSX.Element => {
  const [open, setOpen] = useState(false)
  return (
    <details className="at-item" onToggle={(event) => setOpen(event.currentTarget.open)}>
      <summary>
        <b>{one.title}</b> <span className="muted cp-small">{one.kind}</span>{" "}
        {one.kind !== "link" && (
          <a className="cp-log" href={attachmentFileHref(one.id)} target="_blank" rel="noreferrer" onClick={(event) => event.stopPropagation()}>
            file ↗
          </a>
        )}
      </summary>
      {open && <AttachmentBody one={one} />}
    </details>
  )
}

export const AttachmentList = ({ listed, heading }: {
  readonly listed: readonly AttachmentListed[]
  readonly heading: string
}): JSX.Element => (
  <section className="cp-panel">
    <h3>{heading} <span className="count">{listed.length}</span></h3>
    {listed.length === 0 ? <p className="muted">none</p> : ordered(listed).map((one) => <Folded key={one.id} one={one} />)}
  </section>
)
