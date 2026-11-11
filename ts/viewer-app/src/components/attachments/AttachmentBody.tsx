/** One attachment read and drawn by its kind's component. */
import type { JSX } from "react"
import { api, type AttachmentListed, type AttachmentView, attachmentFileHref, useAsync } from "../../api.ts"
import { DiffAttachment } from "./DiffAttachment.tsx"
import { FileAttachment } from "./FileAttachment.tsx"
import { JsonAttachment } from "./JsonAttachment.tsx"
import { LinkAttachment } from "./LinkAttachment.tsx"
import { LogAttachment } from "./LogAttachment.tsx"
import { RecordsAttachment } from "./RecordsAttachment.tsx"
import { TableAttachment } from "./TableAttachment.tsx"

/** `view` of `one` drawn by its kind's component. */
export const ViewOf = ({ one, view }: { readonly one: AttachmentListed; readonly view: AttachmentView }): JSX.Element => {
  const href = attachmentFileHref(one.id)
  switch (view.kind) {
    case "log":
      return <LogAttachment text={view.text} bytes={view.bytes} href={href} />
    case "records":
      return <RecordsAttachment records={view.records} />
    case "diff":
      return <DiffAttachment diff={view.diff} />
    case "table":
      return <TableAttachment {...view} />
    case "json":
      return <JsonAttachment id={one.id} value={view.value} label={one.name} />
    case "file":
      return <FileAttachment bytes={view.bytes} href={href} name={one.name} />
    case "link":
      return <LinkAttachment url={view.url} />
  }
}

export const AttachmentBody = ({ one }: { readonly one: AttachmentListed }): JSX.Element => {
  const read = useAsync(() => api.attachment(one.id), `attachment#${one.id}`)
  if (read.state === "loading") return <p className="muted">loading…</p>
  if (read.state === "error") return <p className="cp-nok">{one.title}: {read.message}</p>
  return <ViewOf one={one} view={read.value} />
}
