/**
 * The registry entry for a capture that is not worth replaying, offered for
 * copying: pick the reason off the list, take the JSON, paste it into
 * `registry.json`.
 *
 * A capture whose source is plainly broken — truncated, one-sided, holding no
 * call at all — costs a full triage session to say one sentence about, and the
 * sentence is always the same one. This turns that session into two clicks. It
 * only ever offers `never-fix`, the verdict for a capture the corpus should
 * stop asking about: every other verdict names a defect somebody owes a fix
 * for, and a defect is not something a picker can pick.
 *
 * It COPIES rather than writes. The registry is curated knowledge in git and a
 * verdict is a human's to enter; the button removes the typing, not the
 * decision.
 */
import type { JSX } from "react"
import { useState } from "react"

/** One reason a capture is unreplayable, as the record will state it. */
interface Reason {
  /** The word the picker shows: what the reader recognized. */
  readonly label: string
  /** The note the record carries; empty for the free-text entry. */
  readonly note: string
}

/** The picker's free-text entry: a reason the list does not carry. */
const OTHER = "other (type the note)"

/**
 * The reasons that can be READ OFF a capture without a run — the whole point of
 * the shortcut. Each note says what the capture lacks, because that is what a
 * later reader needs: `never-fix` is final, so the note is the only account of
 * the capture that survives.
 */
const REASONS: readonly Reason[] = [
  {
    label: "truncated mid-dialog",
    note: "capture truncated mid-dialog: no complete call to replay, and a synthetic completion " +
      "would be ours rather than the captured platform's"
  },
  {
    label: "source call incomplete",
    note: "the source call is incomplete: the capture never holds the messages a replay would " +
      "have to confront"
  },
  {
    label: "no dialog at the boundary",
    note: "the capture holds no INVITE dialog across the SUT's boundary: there is no call here to " +
      "replay"
  },
  {
    label: "one side only",
    note: "only one side of the SUT's traffic was captured: with the peer leg absent no divergence " +
      "can be stated"
  },
  {
    label: "captured platform misbehaved",
    note: "the divergence is the captured platform breaking its own contract, which is a finding " +
      "about the source and not one a replay can state"
  },
  { label: OTHER, note: "" }
]

/** The reason the picker opens on: the one a broken source shows most often. */
const FIRST = REASONS[0].label

/** Today, as the registry dates a record: ISO date, local calendar. */
const today = (): string => {
  const now = new Date()
  const pad = (n: number): string => String(n).padStart(2, "0")
  return `${now.getFullYear()}-${pad(now.getMonth() + 1)}-${pad(now.getDate())}`
}

/**
 * A path as the registry cites it. Evidence is repo-relative so a record reads
 * the same on every machine; a bundle outside the repo — or one under a root
 * the server did not state — is cited as it stands rather than as a chain of
 * `..`.
 */
const repoRelative = (repoRoot: string | undefined, dir: string): string => {
  if (repoRoot === undefined || repoRoot === "") return dir
  const root = repoRoot.endsWith("/") ? repoRoot : `${repoRoot}/`
  return dir.startsWith(root) ? dir.slice(root.length) : dir
}

/**
 * The map entry, ready to paste: the record's keys sorted and indented as the
 * file holds them, with the trailing comma the paste needs.
 */
export const ignoreRecordText = (
  { capture, bundle, step, note, decidedOn }: {
    capture: string
    bundle: string
    step: string | undefined
    note: string
    decidedOn: string
  }
): string => {
  const record = {
    decided_on: decidedOn,
    evidence: { bundle, note, ...(step === undefined ? {} : { step }) },
    verdict: "never-fix"
  }
  const body = JSON.stringify(record, null, 2).split("\n").join("\n  ")
  return `  ${JSON.stringify(capture)}: ${body},`
}

export const IgnoreRecord = (
  { capture, repoRoot, evidenceDir, step }: {
    capture: string
    /** Absent from an older server than this page: the path is then cited whole. */
    repoRoot: string | undefined
    /**
     * What the record cites as evidence, absolute: the cell on screen, else the
     * capture's own workdir. A capture the cut refused has no bundle and still
     * earns a record, so the coordinate falls back rather than going missing.
     */
    evidenceDir: string | undefined
    /** The step the run died at, cited as the evidence coordinate. */
    step: string | undefined
  }
): JSX.Element => {
  const [pick, setPick] = useState(FIRST)
  const [typed, setTyped] = useState("")
  const [copied, setCopied] = useState(false)
  const note = pick === OTHER ? typed : REASONS.find((entry) => entry.label === pick)?.note ?? ""
  const bundle = evidenceDir === undefined ? "" : repoRelative(repoRoot, evidenceDir)
  const text = ignoreRecordText({ capture, bundle, step, note, decidedOn: today() })
  const ready = note.trim() !== "" && bundle !== ""

  return (
    <details className="study-ignore">
      <summary>ignore this capture — copy a never-fix record</summary>
      <p className="muted">
        for a capture whose SOURCE is plainly unreplayable, read off the pcap rather than proven by
        a run. Paste the entry into the triage registry, keeping its keys sorted. Every other
        verdict names a defect and still owes a triage session.
      </p>
      <div className="ignore-controls">
        <select
          value={pick}
          onChange={(event) => {
            setPick(event.target.value)
            setCopied(false)
          }}
        >
          {REASONS.map((entry) => <option key={entry.label} value={entry.label}>{entry.label}</option>)}
        </select>
        {pick === OTHER && (
          <input
            type="text"
            className="ignore-note"
            placeholder="why this capture is not worth replaying"
            value={typed}
            onChange={(event) => {
              setTyped(event.target.value)
              setCopied(false)
            }}
          />
        )}
        <button
          type="button"
          className="copy-ref"
          disabled={!ready}
          title={ready ? `copy the record for ${capture}` : "a record needs a note and an evidence path"}
          onClick={() => {
            void navigator.clipboard.writeText(text).then(() => {
              setCopied(true)
              window.setTimeout(() => setCopied(false), 1600)
            })
          }}
        >
          {copied ? "copied" : "copy record"}
        </button>
      </div>
      {bundle === "" && (
        <p className="error">
          nothing on disk to cite: a record's evidence path cannot be empty, so this one is written
          by hand.
        </p>
      )}
      <pre className="ignore-preview">{text}</pre>
    </details>
  )
}
