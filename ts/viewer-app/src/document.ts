/** Reading a served document as a v3 pivot, or saying why it is not one. */
import type { PivotV3 } from "@sip/viewer-core"

export type DocumentRead =
  | { readonly ok: true; readonly document: PivotV3 }
  | { readonly ok: false; readonly reason: string }

export const readPivot = (value: unknown): DocumentRead => {
  if (typeof value !== "object" || value === null) return { ok: false, reason: "not a JSON object" }
  const version = (value as Record<string, unknown>)["pivot_version"]
  if (version !== 3) {
    return {
      ok: false,
      reason: `pivot_version is ${JSON.stringify(version)}, and the viewer draws v3 documents`
    }
  }
  return { ok: true, document: value as PivotV3 }
}
