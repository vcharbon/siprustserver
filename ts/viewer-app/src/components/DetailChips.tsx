/**
 * A record's bookkeeping as one line of `key value` chips: each chip is cut
 * to fit, the full value is its tooltip.
 */
import type { JSX } from "react"

const chipValue = (value: unknown): string =>
  typeof value === "string" ? value : JSON.stringify(value) ?? "undefined"

/** Past this many characters a chip gives up width before the short ones do. */
const LONG = 20

/** The record's scalar fields: what a chip can hold. */
export const scalarFields = (record: unknown): ReadonlyArray<readonly [string, unknown]> =>
  typeof record === "object" && record !== null && !Array.isArray(record)
    ? Object.entries(record).filter(([, value]) =>
      typeof value === "string" || typeof value === "number" || typeof value === "boolean"
    )
    : []

export const DetailChips = ({ fields }: { fields: ReadonlyArray<readonly [string, unknown]> }): JSX.Element => (
  <div className="sip-meta">
    {fields.map(([key, value]) => (
      <span
        key={key}
        className={chipValue(value).length > LONG ? "sip-meta-item long" : "sip-meta-item"}
        title={`${key}: ${chipValue(value)}`}
      >
        <span className="sip-meta-key">{key}</span>
        <span className="sip-meta-value">{chipValue(value)}</span>
      </span>
    ))}
  </div>
)
