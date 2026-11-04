/**
 * A campaign cell's CDRs in its call details: the counts its verdict states,
 * each call's declared raw CDR lines beside the lines seen, then each CDR
 * file its bundle holds, folded, downloadable as it is, one block per CDR
 * (its fields, its tables, the whole as JSON where it is one).
 */
import type { JSX } from "react"
import { api, campaignCellPath, type CampaignDeclaredLines, type CdrBlock, useAsync } from "../api.ts"
import { callLines } from "../campaign-cdr.ts"
import { JsonView } from "./JsonView.tsx"

const Block = ({ block }: { readonly block: CdrBlock }): JSX.Element => (
  <details className="cp-cdr">
    <summary className="mono">{block.title}</summary>
    {block.fields.length > 0 && (
      <table className="cp-table cp-cdr-fields">
        <tbody>
          {block.fields.map((field, at) => (
            <tr key={at}>
              <td className="muted mono">{field.name}</td>
              <td className="mono cp-expected">{field.value}</td>
            </tr>
          ))}
        </tbody>
      </table>
    )}
    {block.tables.map((table) => (
      <div key={table.title}>
        <h4>{table.title} <span className="muted">{table.rows.length}</span></h4>
        <table className="cp-table cp-cdr-fields">
          <thead>
            <tr>{table.columns.map((column) => <th key={column}>{column}</th>)}</tr>
          </thead>
          <tbody>
            {table.rows.map((row, at) => (
              <tr key={at}>{row.map((value, i) => <td key={i} className="mono">{value}</td>)}</tr>
            ))}
          </tbody>
        </table>
      </div>
    ))}
    {block.json !== null && <JsonView value={block.json} label="json" />}
  </details>
)

export const CellCdrs = ({ capture, session, cell, declared }: {
  readonly capture: string
  readonly session: string
  readonly cell: string
  readonly declared: readonly CampaignDeclaredLines[] | undefined
}): JSX.Element => {
  const read = useAsync(() => api.campaignCdr(capture, session, cell), `${capture}/${session}/${cell}`)
  if (read.state === "loading") return <p className="muted">loading…</p>
  if (read.state === "error") return <p className="cp-nok">CDRs: {read.message}</p>
  const cdr = read.value
  const calls = callLines(declared, cdr.lines)
  return (
    <section className="cp-panel">
      <h3>
        CDRs{" "}
        <span className="muted cp-small">
          {cdr.facts.map((fact) => `${fact.name} ${fact.value}`).join(" · ")}
        </span>
      </h3>
      {calls.length > 0 && (
        <table className="cp-table cp-cdr-calls">
          <thead>
            <tr><th>call</th><th>Call-ID</th><th title="raw CDR lines the case declares">declared</th><th>seen</th></tr>
          </thead>
          <tbody>
            {calls.map((call, at) => (
              <tr key={at}>
                <td className="mono">{call.call ?? "—"}</td>
                <td className="mono">{call.callId ?? call.leg}</td>
                <td>{call.declared ?? "—"}</td>
                <td className={call.seen === null ? "muted" : call.declared === null || call.declared === call.seen ? "cp-ok" : "cp-nok"}>
                  {call.seen ?? "—"}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      {cdr.files.length === 0 && <p className="muted">none</p>}
      {cdr.files.map((file) => (
        <details key={file.name} className="cp-cdr-file">
          <summary>
            <b>{file.label}</b> <span className="muted">{file.blocks.length}</span>{" "}
            <a
              className="cp-log"
              href={`${campaignCellPath(capture, session, cell)}/files/${encodeURIComponent(file.name)}`}
              download={file.name}
              onClick={(event) => event.stopPropagation()}
            >
              {file.name} ↓
            </a>
          </summary>
          {file.note !== null && <p className="cp-nok">{file.note}</p>}
          {file.blocks.map((block, at) => <Block key={at} block={block} />)}
        </details>
      ))}
    </section>
  )
}
