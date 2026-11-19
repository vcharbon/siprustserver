/**
 * The generic rows' order (ADR-0062 item 6): their sections, in match order,
 * and each row's place in its section. First match wins, so where two rows
 * name one probe the earlier one rules it; `test/overlap.golden.json` pins
 * every such winner. A deployment places these rows in its own sections, its
 * order restricted to them equal to {@link RFC_ROWS}.
 *
 * - `keepalive` — the in-dialog OPTIONS exchange the answering stack terminates.
 * - `stack-owned` — a value the sending stack allocates or addresses itself, the dialog's own addresses, and its session's continuity.
 * - `sender-owned` — a value its sender states for itself, not a fact about the call.
 * - `capability` — the sender's own advertisements, and those RFC 3261 §20 makes mandatory or not applicable.
 * - `captured-only` — the capture carried it and no relay could have, or the run never drove it in.
 * - `shape` — a message the stack adds, leaves out or answers on its own, where the RFC leaves it to the stack or to arrival order or the captured side broke the clause that states it.
 */
import { type DeltaRule, section } from "@sip/delta-rules/rule"
import * as Capability from "./capability.js"
import * as CapturedOnly from "./captured-only.js"
import type { RfcFacts } from "./facts.js"
import * as Keepalive from "./keepalive.js"
import type { RfcRow } from "./row.js"
import * as SenderOwned from "./sender-owned.js"
import * as Shape from "./shape.js"
import * as StackOwned from "./stack-owned.js"

/** A section of the generic rows. */
type RfcSection = "keepalive" | "stack-owned" | "sender-owned" | "capability" | "captured-only" | "shape"

/** The sections, in match order, each with its rows in order. */
export const RFC_SECTIONS: ReadonlyArray<{ readonly name: RfcSection; readonly rows: ReadonlyArray<RfcRow> }> = [
  {
    name: "keepalive",
    rows: [
      Keepalive.keepaliveExchangeLocallyTerminated,
      Keepalive.keepaliveExchangeRelayedEndToEnd,
      Keepalive.keepaliveExchangeAdvertisement,
      Keepalive.keepaliveExchangeChargingVector,
      Keepalive.keepaliveExchangeHeaderStated
    ]
  },
  {
    name: "stack-owned",
    rows: [
      StackOwned.viaStackOwned,
      StackOwned.callIdStackOwned,
      StackOwned.contentLengthStackOwned,
      StackOwned.multipartBoundaryStackOwned,
      StackOwned.maxForwardsStackOwned,
      StackOwned.cseqNumberStackOwned,
      StackOwned.rseqStackOwned,
      StackOwned.rackCseqStackOwned,
      StackOwned.contactHostIsReplaySocket,
      StackOwned.contactAdvertisedOnLocallyAnswered,
      StackOwned.fromToTagStackOwned,
      StackOwned.dialogDisplayNameDropped,
      StackOwned.callerDialogAddressKept,
      StackOwned.sdpOriginStackOwned,
      StackOwned.sdpSessionIdRestated,
      StackOwned.sdpVersionStepOne
    ]
  },
  {
    name: "sender-owned",
    rows: [
      SenderOwned.referProgressNotifyState,
      SenderOwned.releaseCauseValueNotCompared,
      SenderOwned.sessionRefreshIntervalWithheld,
      SenderOwned.chargingVectorOwnReinviteTowardCallerVaries,
      SenderOwned.timestampRestated,
      SenderOwned.responseTimestampEchoed,
      SenderOwned.reasonStatedOnCancel
    ]
  },
  {
    name: "capability",
    rows: [
      Capability.capabilitySetRelayedPastRelease,
      Capability.methodNotAllowedStatesAllow,
      Capability.failureFinalAllowEventsRelayed,
      Capability.capabilitySetAddedByStack,
      Capability.locallyAnsweredNonInviteAdvertisement,
      Capability.capabilitySetInertOnTransactionDerived,
      Capability.capabilitySetNotGenerated
    ]
  },
  {
    name: "captured-only",
    rows: [
      CapturedOnly.selfDateRegenerated,
      CapturedOnly.selfTimestampRegenerated,
      CapturedOnly.mintedFinalTimestampEchoed,
      CapturedOnly.mintedGlareRejectionRelaysNothing,
      CapturedOnly.glareRejectionRetryAfterNotGenerated,
      CapturedOnly.accessNetworkContextNotGenerated,
      CapturedOnly.identityAssertionNotGenerated,
      CapturedOnly.privacyInstructionNotGenerated,
      CapturedOnly.releaseCauseNotGenerated,
      CapturedOnly.bodyDescriptorDroppedWithItsBody,
      CapturedOnly.bodyMetadataNotGenerated,
      CapturedOnly.invitationExpiryNotGenerated,
      CapturedOnly.relayInputNotDriven
    ]
  },
  {
    name: "shape",
    rows: [
      Shape.prackAutomaticStray,
      Shape.byeTeardownCrossStray,
      Shape.tryingOnNonInviteNotReproduced,
      Shape.byeNotRelayedOntoCancelledEarlyDialog,
      Shape.cancelledCallNotDialled,
      Shape.cancelPastAnswerNotRelayed,
      Shape.answerRelayedAheadOfCrossingCancel,
      Shape.referProgressNotifyReport,
      Shape.tryingProvisionalUnscripted,
      Shape.tryingOmittedBeforeFinal,
      Shape.requestPastReleaseAnswered481
    ]
  }
]

/** Every generic row, each once, in match order. */
export const RFC_ROWS: ReadonlyArray<RfcRow> = RFC_SECTIONS.flatMap((s) => s.rows)

/** The generic rows built in their own sections: what their overlap golden file is read over. */
export const RFC_RULES: ReadonlyArray<DeltaRule<RfcFacts, RfcSection>> = RFC_SECTIONS.flatMap((s) => section(s.name, s.rows))
