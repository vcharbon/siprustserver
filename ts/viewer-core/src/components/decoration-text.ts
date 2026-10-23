/** Badge text for a decoration, plus the visual weight it carries. */
import type { Decoration } from "../layout/model.ts"

export type BadgeTone = "neutral" | "info" | "warn" | "alert"

export interface Badge {
  readonly text: string
  readonly tone: BadgeTone
  readonly title: string
}

export const decorationBadge = (decoration: Decoration): Badge | undefined => {
  switch (decoration.kind) {
    case "delay":
      return decoration.ms === 0 ? undefined : {
        text: `+${decoration.ms}ms`,
        tone: decoration.timerLinked ? "warn" : "neutral",
        title: `${decoration.ms}ms from ${decoration.from}${
          decoration.timerLinked ? ", timer-linked" : ""
        }${decoration.compressible ? ", compressible" : ", not compressible"}`
      }
    case "auto":
      return { text: "auto", tone: "neutral", title: "the interpreter's own stack owns this message" }
    case "optional":
      return { text: "optional", tone: "info", title: "tolerated absence" }
    case "check":
      return decoration.mode === "assert"
        ? undefined
        : { text: "record", tone: "neutral", title: "recorded for post-run confrontation, matched against nothing" }
    case "retransmits":
      return { text: `x${decoration.count + 1}`, tone: "warn", title: `${decoration.count} retransmission(s)` }
    case "deviation":
      return { text: decoration.deviationKind, tone: "alert", title: `deviation ${decoration.deviationId}` }
    case "after":
      return { text: `after ${decoration.refs.join(", ")}`, tone: "info", title: "cross-leg ordering" }
    case "checks":
      return { text: `${decoration.count} checks`, tone: "info", title: "inline field assertions" }
    case "within":
      return { text: `<=${decoration.ms}ms`, tone: "info", title: "per-step expect budget" }
    case "overlap":
      return { text: `races ${decoration.with}`, tone: "warn", title: "declared race" }
    case "early":
      return { text: `early ${decoration.id}`, tone: "info", title: "early dialog id" }
    case "body":
      return { text: `body ${decoration.summary}`, tone: "neutral", title: "body" }
    case "observed":
      return { text: `${(decoration.atUs / 1000).toFixed(1)}ms`, tone: "neutral", title: "capture/run offset" }
    case "defect":
      return { text: "defect", tone: "alert", title: decoration.description ?? "the step whose outcome IS the defect" }
    case "step":
      return { text: decoration.id, tone: "info", title: "the flow step the runner attributed this to" }
  }
}

export const decorationBadges = (decorations: readonly Decoration[]): readonly Badge[] =>
  decorations.flatMap((decoration) => {
    const badge = decorationBadge(decoration)
    return badge ? [badge] : []
  })
