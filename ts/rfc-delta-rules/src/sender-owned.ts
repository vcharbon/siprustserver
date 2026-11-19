/**
 * The `sender-owned` section: a value its sender states for itself (a cause,
 * a clock reading, an interval it negotiates, a charging vector of a request
 * it originates, the state of a subscription it notifies), never a fact about
 * the call the replay must reproduce.
 */
import { itemsOf, onlyCaptured, onlyReplayed, type Side, single, valueDiffers } from "@sip/delta-rules/header-items"
import { amongInboundValues } from "@sip/delta-rules/provenance"
import { inDialogRequest, isRequest, onLeg } from "@sip/delta-rules/scope"
import { reasonForm, reasonKey, timestampOf } from "@sip/delta-rules/value-forms"
import { SESSION_TIMER } from "./header-lists.js"
import type { RfcRow } from "./row.js"

export const referProgressNotifyState: RfcRow = {
  id: "refer-progress-notify-state",
  on: { kind: "header", family: ["subscription-state"], scope: ["NOTIFY"] },
  gate: "referOutcomeAtStep",
  rfc: "RFC 3515 §2.4.4, RFC 6665 §4.1.3",
  why:
    "The Subscription-State of the one NOTIFY the document scripts for a REFER: RFC 3515 §2.4.5 " +
    "leaves the stack free to send a progress NOTIFY (`pending` or `active`) before the terminal " +
    "one, which the run shows following on that leg, where the capture's states `terminated`.",
  header: (h, facts) => {
    if (!isRequest(h.scope, "NOTIFY") || !facts.referOutcomeAtStep.has(h.step)) return false
    const state = (side: Side): string => (single(h, side) ?? "").split(";")[0]!.trim().toLowerCase()
    return state("captured") === "terminated" && ["pending", "active"].includes(state("replayed"))
  }
}

export const releaseCauseValueNotCompared: RfcRow = {
  id: "release-cause-value-not-compared",
  on: { kind: "header", family: ["reason"] },
  rfc: "RFC 3326 §2",
  why:
    "A Reason both sides carry item for item with the same protocol, cause and parameters, " +
    "rendered differently: spacing around `;` and `=`, a parameter's case or quoting (RFC 3326 " +
    "§2). Another cause, or a parameter added or stripped, stays visible.",
  header: (h) => {
    if (!valueDiffers(h)) return false
    const a = itemsOf(h, "captured").map(reasonForm).sort()
    const b = itemsOf(h, "replayed").map(reasonForm).sort()
    return a.every((item, i) => item === b[i])
  }
}

export const sessionRefreshIntervalWithheld: RfcRow = {
  id: "session-refresh-interval-withheld",
  on: { kind: "header", family: SESSION_TIMER },
  rfc: "RFC 4028 §7",
  why:
    "A Session-Expires / Min-SE the capture carried and the relay sends none of: RFC 4028 §7 " +
    "makes the refresher and interval a per-leg negotiation, so relaying either would bind " +
    "this peer to the other leg's. An interval the replay mints stays visible.",
  header: (h) => onlyCaptured(h)
}

export const chargingVectorOwnReinviteTowardCallerVaries: RfcRow = {
  id: "charging-vector-own-reinvite-toward-caller-varies",
  on: { kind: "header", family: ["p-charging-vector"], scope: ["INVITE"] },
  rfc: "RFC 7315 §4.6, §5.6",
  why:
    "A re-INVITE toward the caller that each side sent on its own behalf, where the replay " +
    "carries the caller's own vector or none and the capture another: a vector minted for that " +
    "request, its 18x's, the callee's, or none (RFC 7315 §4.6, §5.6).",
  header: (h) => {
    if (!onLeg(h, ["caller"]) || !inDialogRequest(h.scope, "INVITE") || h.driven !== undefined) return false
    const replayed = itemsOf(h, "replayed")
    const captured = itemsOf(h, "captured")
    const same = captured.length === replayed.length && captured.every((v) => replayed.includes(v))
    return !same && amongInboundValues(h, replayed)
  }
}

export const timestampRestated: RfcRow = {
  id: "timestamp-restated",
  on: { kind: "header", family: ["timestamp"] },
  rfc: "RFC 3261 §8.2.6.1, §20.38",
  why:
    "A Timestamp both sides carry where the replay's is the stack's own: on a request a reading of " +
    "its clock no one handed it (RFC 3261 §20.38), on a response the echo of the request it " +
    "answers (§8.2.6.1), no delay. Another grammar or a carried value stays visible.",
  header: (h) => {
    const replayed = itemsOf(h, "replayed").map((v) => v.trim())
    if (itemsOf(h, "captured").length === 0 || replayed.length === 0) return false
    if (!replayed.every((v) => /^[0-9]+(\.[0-9]*)?$/.test(v))) return false
    if (h.scope.kind !== "response") {
      const handed = new Set(h.inboundValues.map(timestampOf))
      return replayed.every((v) => !handed.has(v))
    }
    const answered = new Set(h.inAnsweredRequest.map(timestampOf))
    return replayed.every((v) => answered.has(v))
  }
}

export const responseTimestampEchoed: RfcRow = {
  id: "response-timestamp-echoed",
  on: { kind: "header", family: ["timestamp"] },
  rfc: "RFC 3261 §8.2.6.1, §20.38",
  why:
    "A Timestamp the replay states on a response and the capture did not, other than a 100 " +
    "Trying copying the request's: RFC 3261 §20.38 makes the echo on any other response a MAY.",
  header: (h) => h.scope.kind === "response" && onlyReplayed(h)
}

export const reasonStatedOnCancel: RfcRow = {
  id: "reason-stated-on-cancel",
  on: { kind: "header", family: ["reason"], scope: ["CANCEL"] },
  rfc: "RFC 3326 §2",
  why:
    "A CANCEL whose Reason holds every cause the capture's held and more: a cause of the stack's " +
    "own, or a caller's cause the captured side dropped and the stack relayed (RFC 3326 §2). " +
    "Every captured (protocol, cause) is still there, so a dropped one stays visible.",
  header: (h) => {
    const replayed = itemsOf(h, "replayed").map(reasonKey)
    return isRequest(h.scope, "CANCEL") && itemsOf(h, "captured").every((item) => replayed.includes(reasonKey(item)))
  }
}
