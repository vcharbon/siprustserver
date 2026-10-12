/**
 * The source panel: the capture a generated case was cut from, drawn
 * uninterpreted beside the pivot ladder.
 *
 * It is a TRIAGE view — the WHOLE capture by default, colour-coded by Call-ID
 * family, with the legs this case selected in full colour and the rest pale. A
 * case with no flows document says so; absence is an answer here, so nothing in
 * this pane treats it as an error.
 *
 * The capture is read a WINDOW of legs at a time, opening on the legs the case
 * cites: a capture of thousands of calls has a document no response carries.
 */
import {
  buildFlowsLayout,
  CaptureLegend,
  captureSelection,
  FlowsLadder,
  parseFlowsDocument,
  type FlowsRow,
  type PivotV3
} from "@sip/viewer-core"
import type { JSX } from "react"
import { useMemo, useState } from "react"
import { api, useAsync, type CaseSource } from "../api.ts"
import { useCaseFlows, type CaseFlows } from "../case-flows.ts"
import { useCaptureFilters } from "./capture-filters.ts"
import { WindowLine } from "./WindowLine.tsx"

export interface SourceLadderProps {
  readonly source: CaseSource
  readonly pivot: PivotV3 | undefined
  /** The window of the capture on screen, and how to move it. */
  readonly flows: CaseFlows
}

export const SourceLadder = ({ source, pivot, flows }: SourceLadderProps): JSX.Element => {
  const [selected, setSelected] = useState<FlowsRow | undefined>(undefined)
  const [groupsOnly, setGroupsOnly] = useState(false)
  const filters = useCaptureFilters()
  const read = useMemo(() => parseFlowsDocument(flows.document), [flows.document])
  const selection = useMemo(() => (pivot ? captureSelection(pivot) : undefined), [pivot])
  const layout = useMemo(
    () =>
      read.ok
        ? buildFlowsLayout(read.document, {
          groups: source.callGroups,
          restrictToGroups: groupsOnly,
          selection,
          legs: filters.legs.value,
          sockets: filters.sockets.value,
          title: source.capture ?? "capture source"
        })
        : undefined,
    [read, source, selection, groupsOnly, filters.legs.value, filters.sockets.value]
  )
  if (!read.ok) return <p className="error">{source.path}: {read.reason}</p>
  return (
    <section className="source-pane">
      <p className="muted path">{source.path}</p>
      {layout && (
        <>
          <p className="muted">
            {layout.subtitle}
            {source.callGroups.length > 0 && ` · case call groups ${source.callGroups.join(", ")}`}
          </p>
          <label className="source-toggle">
            <input
              type="checkbox"
              checked={groupsOnly}
              onChange={(event) => setGroupsOnly(event.target.checked)}
              disabled={source.callGroups.length === 0}
            />
            case groups only (off: the whole capture, which is what triage reads)
          </label>
          <WindowLine window={{ ...flows.window, beyond: flows.beyond, total: source.legs, onWindow: flows.onWindow }} />
          {flows.error !== undefined && <p className="error">{flows.error}</p>}
          <CaptureLegend
            layout={layout}
            sockets={filters.sockets.value}
            onToggleSocket={filters.sockets.onToggle}
            legs={filters.legs.value}
            onToggleLeg={filters.legs.onToggle}
            selectedTag={filters.tag.value}
            onSelectTag={(tag) => (tag === filters.tag.value ? filters.tag.onClear() : filters.tag.onSelect(tag))}
          />
          {layout.warnings.length > 0 && (
            <ul className="warnings">
              {layout.warnings.slice(0, 6).map((warning) => <li key={warning}>{warning}</li>)}
            </ul>
          )}
          <div className="ladder-scroll">
            <FlowsLadder
              layout={layout}
              selectedId={selected?.id}
              onSelect={(row) => setSelected(row)}
              selectedTag={filters.tag.value}
              onSelectTag={(tag) => (tag === filters.tag.value ? filters.tag.onClear() : filters.tag.onSelect(tag))}
              legs={filters.legs.value}
              onToggleLeg={filters.legs.onToggle}
            />
          </div>
        </>
      )}
      <h3>
        {selected
          ? `raw · leg ${selected.legIndex} msg ${selected.message.msgIndex}${
            selected.stepId === undefined
              ? selected.selected ? "" : " · leg discarded by this case"
              : ` · step ${selected.stepId}`
          }`
          : "raw"}
      </h3>
      <pre className="raw">
        {selected?.message.raw === undefined || selected.message.raw === ""
          ? "click a message in the source ladder to read its bytes"
          : selected.message.raw}
      </pre>
    </section>
  )
}

/** The ladder over the window this case opens on: one read of the source, then windows. */
const Windowed = (
  { id, variant, source, pivot }: {
    id: string
    variant: string
    source: CaseSource
    pivot: PivotV3 | undefined
  }
): JSX.Element => {
  const flows = useCaseFlows(id, variant, source)
  return <SourceLadder source={source} pivot={pivot} flows={flows} />
}

export const SourcePane = (
  { id, variant, pivot }: { id: string; variant: string; pivot: PivotV3 | undefined }
): JSX.Element => {
  const source = useAsync(() => api.caseSource(id, variant), `${id}#${variant}#source`)
  if (source.state === "loading") return <p className="muted">loading the capture source…</p>
  if (source.state === "error") return <p className="error">{source.message}</p>
  if (!source.value.available) {
    return (
      <p className="muted">
        source not available — {source.value.detail}.
      </p>
    )
  }
  return <Windowed id={id} variant={variant} source={source.value} pivot={pivot} />
}
