/**
 * The per-message delta sheet: what the replay put on the wire against what the
 * capture did, one row per compared signature, classified.
 *
 * Every row names the STEP it was taken at, and that name is a control: it
 * takes the ladders to the message, so a row of the table and the arrows it
 * rules on are never read apart. A ladder click travels the other way and marks
 * this table's rows for the same step.
 */
import type { JSX } from "react"
import { useEffect, useRef } from "react"

/** A confrontation row, read loosely: the server decoded it, this only renders it. */
export interface ConfrontationRow {
  readonly step: string
  readonly kind: string
  readonly signature: string
  readonly name: string
  readonly captured: readonly string[]
  readonly replayed: readonly string[]
  readonly klass: string
  readonly rule: string
}

const record = (value: unknown): Record<string, unknown> | undefined =>
  typeof value === "object" && value !== null ? value as Record<string, unknown> : undefined

const str = (value: unknown): string | undefined => typeof value === "string" ? value : undefined

export const readConfrontationRow = (value: unknown): ConfrontationRow => {
  const row = record(value) ?? {}
  const strings = (field: unknown): readonly string[] =>
    Array.isArray(field) ? field.filter((entry): entry is string => typeof entry === "string") : []
  return {
    step: str(row["step"]) ?? "",
    kind: str(row["kind"]) ?? "",
    signature: str(row["signature"]) ?? "",
    name: str(row["name"]) ?? "",
    captured: strings(row["captured"]),
    replayed: strings(row["replayed"]),
    klass: str(row["class"]) ?? "unknown",
    rule: str(row["rule"]) ?? ""
  }
}

export interface ConfrontationProps {
  readonly rows: readonly unknown[]
  /** The step the ladders are on; its rows are marked and scrolled to. */
  readonly focusedStep: string | undefined
  readonly onPickStep: (step: string) => void
}

/** A compared side: a body row's text keeps its line structure, every other row lists its values. */
const side = (row: ConfrontationRow, values: readonly string[]): JSX.Element =>
  row.kind === "body"
    ? <pre className="mono body-text">{values.join("\n") || "—"}</pre>
    : <>{values.join(" | ") || "—"}</>

export const Confrontation = ({ rows, focusedStep, onPickStep }: ConfrontationProps): JSX.Element => {
  const read = rows.map(readConfrontationRow)
  const box = useRef<HTMLDivElement | null>(null)
  const firstFocused = useRef<HTMLTableRowElement | null>(null)
  // The table scrolls in its OWN box. A focused row must never take the page
  // with it: the ladders the reader just clicked would leave the screen.
  useEffect(() => {
    const scroll = box.current
    const row = firstFocused.current
    if (scroll === null || row === null) return
    scroll.scrollTo({ top: Math.max(0, row.offsetTop - scroll.clientHeight / 2) })
  }, [focusedStep])

  let marked = false
  return (
    <div className="confrontation-scroll" ref={box}>
      <table className="listing confrontation-listing">
        <thead>
          <tr>
            <th>step</th>
            <th>kind</th>
            <th>signature</th>
            <th>captured</th>
            <th>replayed</th>
            <th>class</th>
            <th>rule</th>
          </tr>
        </thead>
        <tbody>
          {read.map((row, index) => {
            const focused = row.step !== "" && row.step === focusedStep
            const first = focused && !marked
            if (first) marked = true
            return (
              <tr
                key={index}
                className={`class-${row.klass}${focused ? " row-focused" : ""}`}
                ref={first ? firstFocused : undefined}
              >
                <td className="mono">
                  {row.step === ""
                    ? "—"
                    : (
                      <button
                        type="button"
                        className="step-link"
                        title={`show ${row.step} on the ladders`}
                        onClick={() => onPickStep(row.step)}
                      >
                        {row.step}
                      </button>
                    )}
                </td>
                <td>{row.kind}</td>
                <td className="mono break">{row.signature}</td>
                <td className="mono break">{side(row, row.captured)}</td>
                <td className="mono break">{side(row, row.replayed)}</td>
                <td>
                  <span className={`tag class-${row.klass}`}>{row.klass}</span>
                </td>
                <td className="mono break">{row.rule || "—"}</td>
              </tr>
            )
          })}
        </tbody>
      </table>
    </div>
  )
}
