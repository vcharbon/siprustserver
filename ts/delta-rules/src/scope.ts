/**
 * Where a probe sits: the message it rides (request or response, its method
 * and status) and the role of its leg.
 */
import type { Probe } from "@sip/pipeline"

type MsgScope = Probe.MsgScope

/** The status of a response, to `method` (its CSeq method) where named; `undefined` off one. */
export const statusOf = (scope: MsgScope | undefined, method?: string): number | undefined =>
  scope?.kind === "response" && (method === undefined || scope.cseqMethod === method)
    ? scope.status
    : undefined

/** A request, of `method` where named. */
export const isRequest = (scope: MsgScope | undefined, method?: string): boolean =>
  scope?.kind === "request" && (method === undefined || scope.method === method)

/** An in-dialog request, of `method` where named. */
export const inDialogRequest = (scope: MsgScope | undefined, method?: string): boolean =>
  scope?.kind === "request" && scope.inDialog && (method === undefined || scope.method === method)

/** The initial INVITE, or an INVITE request inside a dialog. */
export const onAnInvite = (scope: MsgScope): boolean => scope.kind === "initial-invite" || isRequest(scope, "INVITE")

/**
 * Whether `method` refreshes the dialog target — the request classes whose
 * Contact re-states where the sender is reached (RFC 3261 §12.2, RFC 3311,
 * RFC 6665 §8.6, RFC 3515).
 */
export const isTargetRefresh = (method: string): boolean =>
  ["INVITE", "UPDATE", "SUBSCRIBE", "NOTIFY", "REFER", "REGISTER"].includes(method)

/** On the in-dialog OPTIONS keepalive exchange, request or answer. */
export const onKeepalive = (scope: MsgScope): boolean => inDialogRequest(scope, "OPTIONS") || statusOf(scope, "OPTIONS") !== undefined

/** Whether the probe was observed on a leg of one of `roles`. */
export const onLeg = (p: Probe.Probe, roles: ReadonlyArray<string>): boolean =>
  p.leg !== undefined && roles.includes(p.leg.role)
