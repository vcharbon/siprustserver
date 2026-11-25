/**
 * The generic rows' fixtures (`@sip/rfc-delta-rules/test-fixtures`): for each
 * row, the probes it rules (the first row in this package's order that names
 * each) and the same-shape probes it leaves to another row or to no row, in RFC
 * terms. A deployment that places the rows runs them through its placed rules,
 * so a placement that alters a row's predicate fails there.
 */
import { bodyProbe, dropped, headerProbe, minted, shape } from "@sip/delta-rules/test-probes"
import type { Classifier, Probe } from "@sip/pipeline"
import type { RfcFacts } from "../src/facts.js"

/** One probe, with the facts it is read under where it needs any. */
export interface Seen {
  readonly probe: Probe.Probe
  readonly facts?: RfcFacts
}

/**
 * A same-shape probe a row leaves visible: what it is, and the row that names
 * it first in this package's order, or `undefined` where no row does.
 */
interface Visible extends Seen {
  readonly defect: string
  readonly leftTo: string | undefined
}

/** One row's fixture. */
interface RfcFixture {
  readonly id: string
  /** Probes the row rules: no earlier row names them. */
  readonly named: ReadonlyArray<Seen>
  /** Same-shape probes the row leaves visible. */
  readonly visible: ReadonlyArray<Visible>
}

/** A run that recorded nothing. */
const EMPTY_RUN: Classifier.RunShape = {
  messages: 0,
  legs: 0,
  legsTornDownBySystem: 0,
  sentRSeqs: new Map(),
  openers: new Map(),
  receptions: new Map(),
  sent: new Map(),
  byeAheadOfInviteFinal: new Set()
}

/** The facts a probe is read under where it needs none. */
export const NO_FACTS: RfcFacts = {
  acknowledgedRSeqAtStep: new Map(),
  releasedLegSteps: new Set(),
  roundsPastReleaseSteps: new Set(),
  tryingOnNonInviteSteps: new Map(),
  tryingOnEarlyByeSteps: new Set(),
  byeBesideCancelSteps: new Set(),
  cancelPastAnswerSteps: new Set(),
  cancelCrossingAnswerSteps: new Set(),
  referOutcomeAtStep: new Map(),
  untriedInviteSteps: new Set(),
  dialledAfterCancelSteps: new Set(),
  facts: { run: EMPTY_RUN, legOfStep: new Map() }
}

/** {@link NO_FACTS} with the facts `over` states. */
const factsWith = (over: Partial<RfcFacts>): RfcFacts => ({ ...NO_FACTS, ...over })

/**
 * {@link NO_FACTS} with a run's per-leg facts stated: `legs` maps each step to
 * the leg it rides, and the run carries what each leg received, sent and opened with.
 */
const ranWith = (
  legs: Readonly<Record<string, string>>,
  run: {
    readonly receptions?: Readonly<Record<string, ReadonlyArray<Classifier.Reception>>>
    readonly sent?: Readonly<Record<string, ReadonlyArray<Classifier.Reception>>>
    readonly openers?: Readonly<Record<string, ReadonlyArray<{ readonly name: string; readonly value: string }>>>
  }
): RfcFacts =>
  factsWith({
    facts: {
      run: {
        ...EMPTY_RUN,
        receptions: new Map(Object.entries(run.receptions ?? {})),
        sent: new Map(Object.entries(run.sent ?? {})),
        openers: new Map(Object.entries(run.openers ?? {}))
      },
      legOfStep: new Map(Object.entries(legs))
    }
  })

/**
 * The caller who sent her INVITE at s1 on leg A with From `from`, to the
 * address `to`, the run recording it as that leg's dialog-creating INVITE;
 * `steps` ride leg A after it.
 */
const callerSent = (address: { readonly from: string; readonly to?: string }, ...steps: ReadonlyArray<string>): RfcFacts =>
  ranWith(Object.fromEntries([["s1", "A"], ...steps.map((s) => [s, "A"])]), {
    sent: { A: [{ step: "s1", method: "INVITE", cseqMethod: "INVITE", from: address.from }] },
    openers: {
      A: [{ name: "From", value: address.from }, ...(address.to === undefined ? [] : [{ name: "To", value: address.to }])]
    }
  })

/**
 * Leg A's REFER outcome: the transferor's REFER answered 202, the progress
 * NOTIFY its step s11 took (`active`, `100`), then `terminals` terminal NOTIFYs
 * no step claimed reporting 403. The BYE the flow expects next rides
 * `byeLeg` at s15, an earlier expect leg A at s9. `elsewhere` gives leg B a
 * NOTIFY no step claimed too.
 */
const referredOn = (o: { readonly terminals?: number; readonly byeLeg?: string; readonly elsewhere?: boolean } = {}): RfcFacts => {
  const terminal: Classifier.Reception = { method: "NOTIFY", cseqMethod: "NOTIFY", subscriptionState: "terminated", fragStatus: 403 }
  return {
    ...ranWith({ s9: "A", s11: "A", s15: o.byeLeg ?? "A" }, {
      receptions: {
        A: [
          { step: "s11", method: "NOTIFY", cseqMethod: "NOTIFY", subscriptionState: "active", fragStatus: 100 },
          ...Array.from({ length: o.terminals ?? 1 }, () => terminal)
        ],
        ...(o.elsewhere === true ? { B: [terminal] } : {})
      }
    }),
    referOutcomeAtStep: new Map([["s11", 403]])
  }
}
const strayNotify = shape({ shape: "serviced-stray", method: "NOTIFY", action: "auto-reacted" })
const notifyForBye = (at: string) => shape({ shape: "method-substitution", expected: "BYE", observed: "NOTIFY" }, undefined, at)

/** Leg A's session opened by the system's 200 at s8 under sess-id 111; the ACK at s34 comes later. */
const sessionOpenedAt8 = ranWith(
  { s8: "A", s34: "A" },
  { receptions: { A: [{ step: "s8", status: 200, cseqMethod: "INVITE", origin: "o=- 111 100 IN IP4 192.0.2.10" }] } }
)

const inDialog = (method: string): Probe.MsgScope => ({ kind: "request", method, inDialog: true })
const answering = (status: number, cseqMethod: string): Probe.MsgScope => ({ kind: "response", status, cseqMethod })

const onBye = inDialog("BYE")
const onPrack = inDialog("PRACK")
const on180 = answering(180, "INVITE")
const on200 = answering(200, "INVITE")
const on486 = answering(486, "INVITE")
const on491 = answering(491, "INVITE")
const on200Bye = answering(200, "BYE")
const on200Options = answering(200, "OPTIONS")
const on302 = answering(302, "INVITE")

const ALICE = "<sip:+15555550100@192.0.2.10>"

const RELAYED_SET = "INVITE, ACK, PRACK, CANCEL, BYE"

/** The peer's Allow the replay alone carries on a 200 to a NOTIFY at `step`, handed in and driven. */
const relayedAllow = (step: string): Probe.Probe =>
  headerProbe({
    name: "Allow",
    step,
    scope: answering(200, "NOTIFY"),
    replayed: [RELAYED_SET],
    inbound: true,
    inboundValues: [RELAYED_SET],
    driven: true
  })

/** The sipfrag of the NOTIFY at s11: the capture's reports a 603 outcome, the replay's its `100` progress. */
const referReport: Probe.Probe = bodyProbe({
  mediaType: "message/sipfrag",
  step: "s11",
  scope: inDialog("NOTIFY"),
  captured: ["SIP/2.0 603 Declined"],
  replayed: ["SIP/2.0 100 Trying"]
})

/** A `session:o=` row of a verbatim run, marked or not as an origin the system mints. */
const mintedOrigin = (minted: boolean): Probe.Probe =>
  bodyProbe({
    mediaType: "application/sdp",
    scope: { kind: "initial-invite" },
    compare: "sdp",
    captured: ["o=- 1000 1000 IN IP4 192.0.2.10"],
    replayed: ["o=- 7 7 IN IP4 192.0.2.10"],
    sdp: { section: "session", line: "o=", ...(minted ? { mintedOrigin: true as const } : {}) }
  })

const onAck = inDialog("ACK")

/** An in-dialog ACK's `session:o=` row whose captured origin carries sess-id 222. */
const restatedOrigin = (replayed: string): Probe.Probe =>
  bodyProbe({
    mediaType: "application/sdp",
    scope: onAck,
    compare: "sdp",
    step: "s34",
    captured: ["o=- 222 105 IN IP4 192.0.2.10"],
    replayed: [replayed],
    sdp: { section: "session", line: "o=" }
  })

/** An in-dialog ACK's `session:o=` row whose captured sess-version is two above the leg's previous one, the replay's one. */
const steppedOrigin = (readings: { readonly versionStepOne?: true }): Probe.Probe =>
  bodyProbe({
    mediaType: "application/sdp",
    scope: onAck,
    compare: "sdp",
    step: "s34",
    captured: ["o=- 222 107 IN IP4 192.0.2.10"],
    replayed: ["o=- 222 106 IN IP4 192.0.2.10"],
    sdp: { section: "session", line: "o=", ...readings }
  })

/** An in-dialog INVITE's To or From toward the caller at s13, as the capture and the replay carried it. */
const towardCaller = (name: "To" | "From", captured: string, replayed: string): Probe.Probe =>
  headerProbe({ name, step: "s13", scope: inDialog("INVITE"), captured: [captured], replayed: [replayed], leg: { role: "caller", direction: "to-caller" } })

const KEEPALIVE: ReadonlyArray<RfcFixture> = [
  {
    id: "keepalive-exchange-locally-terminated",
    named: [{
      probe: headerProbe({
        name: "Via",
        scope: on200Options,
        captured: ["SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK776asdhds"],
        replayed: ["SIP/2.0/UDP 198.51.100.20:5060;branch=z9hG4bKnashds8"]
      })
    }],
    visible: [{
      defect: "a header outside the stack-composed set the replay states on its own keepalive answer, nobody handing it in",
      leftTo: "keepalive-exchange-header-stated",
      probe: headerProbe({ name: "User-to-User", scope: on200Options, replayed: ["04"] })
    }]
  },
  {
    id: "keepalive-exchange-relayed-end-to-end",
    named: [{
      probe: headerProbe({
        name: "Accept",
        scope: on200Options,
        captured: ["application/sdp"],
        inbound: true,
        inboundValues: ["application/sdp, application/isup"]
      })
    }],
    visible: [{
      defect: "a captured Accept item no peer handed in: the capture's own advertisement, not one relayed end to end",
      leftTo: "keepalive-exchange-advertisement",
      probe: headerProbe({
        name: "Accept",
        scope: on200Options,
        captured: ["application/sdp"],
        inbound: true,
        inboundValues: ["application/isup"]
      })
    }]
  },
  {
    id: "keepalive-exchange-advertisement",
    named: [{ probe: headerProbe({ name: "Accept", scope: on200Options, captured: ["application/sdp"] }) }],
    visible: [{
      defect: "an Accept on the 200 to an in-dialog INFO: off the keepalive exchange",
      leftTo: "capability-set-not-generated",
      probe: headerProbe({ name: "Accept", scope: answering(200, "INFO"), captured: ["application/sdp"] })
    }]
  },
  {
    id: "keepalive-exchange-charging-vector",
    named: [{ probe: headerProbe({ name: "P-Charging-Vector", scope: on200Options, replayed: ["icid-value=b"] }) }],
    visible: [{
      defect: "a P-Charging-Vector on a 2xx to the INVITE: off the keepalive exchange",
      leftTo: undefined,
      probe: headerProbe({ name: "P-Charging-Vector", scope: on200, replayed: ["icid-value=b"] })
    }]
  },
  {
    id: "keepalive-exchange-header-stated",
    named: [{
      probe: headerProbe({ name: "Reason", scope: { kind: "request", method: "OPTIONS", inDialog: true }, captured: ["Q.850;cause=16"], inbound: true })
    }],
    visible: [{
      defect: "a Reason on an out-of-dialog OPTIONS: not the keepalive exchange",
      leftTo: undefined,
      probe: headerProbe({ name: "Reason", scope: { kind: "request", method: "OPTIONS", inDialog: false }, captured: ["Q.850;cause=16"], inbound: true })
    }]
  }
]

const STACK_OWNED: ReadonlyArray<RfcFixture> = [
  {
    id: "via-stack-owned",
    named: [{
      probe: headerProbe({
        name: "Via",
        captured: ["SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK776asdhds"],
        replayed: ["SIP/2.0/UDP 198.51.100.20:5060;branch=z9hG4bKnashds8"]
      })
    }],
    visible: [{
      defect: "a BYE the replay sent with no Via",
      leftTo: undefined,
      probe: headerProbe({ name: "Via", scope: onBye, captured: ["SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK776asdhds"] })
    }]
  },
  {
    id: "call-id-stack-owned",
    named: [{ probe: headerProbe({ name: "Call-ID", captured: ["a84b4c76e66710@192.0.2.10"], replayed: ["f81d4fae7dec@198.51.100.20"] }) }],
    visible: [{
      defect: "a BYE the replay sent with no Call-ID",
      leftTo: undefined,
      probe: headerProbe({ name: "Call-ID", scope: onBye, captured: ["a84b4c76e66710@192.0.2.10"] })
    }]
  },
  {
    id: "content-length-stack-owned",
    named: [
      { probe: headerProbe({ name: "Content-Length", captured: ["142"], replayed: ["158"] }) },
      { probe: headerProbe({ name: "Content-Length", scope: onBye, captured: [], replayed: ["0"] }) }
    ],
    visible: [
      { defect: "a body the replay carries where the capture carried none", leftTo: undefined, probe: headerProbe({ name: "Content-Length", captured: ["0"], replayed: ["158"] }) },
      { defect: "a body the replay dropped", leftTo: undefined, probe: headerProbe({ name: "Content-Length", captured: ["142"], replayed: [] }) }
    ]
  },
  {
    id: "multipart-boundary-stack-owned",
    named: [{
      probe: headerProbe({
        name: "Content-Type",
        captured: ["multipart/mixed;boundary=unique-boundary-1"],
        replayed: ["multipart/mixed;boundary=boundary42"]
      })
    }],
    visible: [
      {
        defect: "a parameter other than the boundary dropped",
        leftTo: undefined,
        probe: headerProbe({
          name: "Content-Type",
          captured: ["multipart/mixed;boundary=a;type=application/sdp"],
          replayed: ["multipart/mixed;boundary=b"]
        })
      },
      {
        defect: "another multipart subtype",
        leftTo: undefined,
        probe: headerProbe({ name: "Content-Type", captured: ["multipart/mixed;boundary=a"], replayed: ["multipart/alternative;boundary=b"] })
      }
    ]
  },
  {
    id: "max-forwards-stack-owned",
    named: [
      { probe: headerProbe({ name: "Max-Forwards", captured: ["70"], replayed: ["69"] }) },
      { probe: headerProbe({ name: "Max-Forwards", scope: on200, captured: ["70"], replayed: [] }) }
    ],
    visible: [{
      defect: "a BYE the replay sent with no Max-Forwards",
      leftTo: undefined,
      probe: headerProbe({ name: "Max-Forwards", scope: onBye, captured: ["70"] })
    }]
  },
  {
    id: "cseq-number-stack-owned",
    named: [{ probe: headerProbe({ name: "CSeq", captured: ["314159 INVITE"], replayed: ["1 INVITE"] }) }],
    visible: [
      { defect: "another method", leftTo: undefined, probe: headerProbe({ name: "CSeq", captured: ["314159 INVITE"], replayed: ["314159 BYE"] }) },
      { defect: "a CSeq the replay dropped", leftTo: undefined, probe: headerProbe({ name: "CSeq", scope: onBye, captured: ["2 BYE"], replayed: [] }) }
    ]
  },
  {
    id: "rseq-stack-owned",
    named: [{ probe: headerProbe({ name: "RSeq", scope: on180, captured: ["988789"], replayed: ["1"] }) }],
    visible: [
      { defect: "an RSeq the replay omits", leftTo: undefined, probe: headerProbe({ name: "RSeq", scope: on180, captured: ["988789"], replayed: [] }) },
      { defect: "an RSeq the capture never carried", leftTo: undefined, probe: headerProbe({ name: "RSeq", scope: on180, captured: [], replayed: ["1"] }) }
    ]
  },
  {
    id: "rack-cseq-stack-owned",
    named: [
      { probe: headerProbe({ name: "RAck", scope: onPrack, captured: ["988789 314159 INVITE"], replayed: ["988789 1 INVITE"] }) },
      {
        probe: headerProbe({ name: "RAck", scope: onPrack, step: "s7", captured: ["988789 314159 INVITE"], replayed: ["4711 1 INVITE"] }),
        facts: factsWith({ acknowledgedRSeqAtStep: new Map([["s7", 4711]]) })
      }
    ],
    visible: [
      {
        defect: "a PRACK of a provisional neither side sent",
        leftTo: undefined,
        probe: headerProbe({ name: "RAck", scope: onPrack, step: "s7", captured: ["988789 314159 INVITE"], replayed: ["4711 1 INVITE"] })
      },
      {
        defect: "a PRACK of another provisional than the actor's",
        leftTo: undefined,
        probe: headerProbe({ name: "RAck", scope: onPrack, step: "s7", captured: ["988789 314159 INVITE"], replayed: ["4712 1 INVITE"] }),
        facts: factsWith({ acknowledgedRSeqAtStep: new Map([["s7", 4711]]) })
      },
      {
        defect: "another method",
        leftTo: undefined,
        probe: headerProbe({ name: "RAck", scope: onPrack, captured: ["988789 314159 INVITE"], replayed: ["988789 1 UPDATE"] })
      }
    ]
  },
  {
    id: "contact-host-is-replay-socket",
    named: [{ probe: headerProbe({ name: "Contact", captured: ["<sip:alice@192.0.2.10:5060>"], replayed: ["<sip:alice@198.51.100.20:5080>"] }) }],
    visible: [{
      defect: "a 3xx redirect target moved onto another host: the Contact there names where to retry, not the replay's socket",
      leftTo: undefined,
      probe: headerProbe({
        name: "Contact",
        scope: on302,
        captured: ["<sip:alice@192.0.2.10:5060>"],
        replayed: ["<sip:alice@198.51.100.20:5080>"]
      })
    }]
  },
  {
    id: "contact-advertised-on-locally-answered",
    named: [{ probe: headerProbe({ name: "Contact", scope: on200Bye, replayed: ["<sip:198.51.100.20:5090>"] }) }],
    visible: [{
      defect: "a Contact the replay states on a 302: a redirect target, never the stack's own",
      leftTo: undefined,
      probe: headerProbe({ name: "Contact", scope: on302, replayed: ["<sip:198.51.100.20:5090>"] })
    }]
  },
  {
    id: "from-to-tag-stack-owned",
    named: [
      { probe: headerProbe({ name: "From", captured: [`${ALICE};tag=1928301774`], replayed: [`${ALICE};tag=a6c85cf`] }) },
      { probe: headerProbe({ name: "To", scope: on200, captured: [`${ALICE};tag=1928301774`], replayed: [`${ALICE};tag=314159`] }) }
    ],
    visible: [
      {
        defect: "a display name dropped beside the tag",
        leftTo: undefined,
        probe: headerProbe({ name: "From", captured: [`"Alice" ${ALICE};tag=1928301774`], replayed: [`${ALICE};tag=a6c85cf`] })
      },
      {
        defect: "a user=phone dropped beside the tag",
        leftTo: undefined,
        probe: headerProbe({
          name: "From",
          captured: ["<sip:+15555550100@192.0.2.10;user=phone>;tag=1928301774"],
          replayed: [`${ALICE};tag=a6c85cf`]
        })
      },
      {
        defect: "a ?headers component added beside the tag",
        leftTo: undefined,
        probe: headerProbe({
          name: "From",
          captured: [`${ALICE};tag=1928301774`],
          replayed: ["<sip:+15555550100@192.0.2.10?Subject=x>;tag=a6c85cf"]
        })
      },
      {
        defect: "another user beside the tag",
        leftTo: undefined,
        probe: headerProbe({ name: "From", captured: [`${ALICE};tag=1928301774`], replayed: ["<sip:+15555550101@192.0.2.10>;tag=a6c85cf"] })
      }
    ]
  },
  {
    id: "dialog-display-name-dropped",
    named: [{
      probe: headerProbe({ name: "From", scope: onBye, captured: ['"Alice" <sip:+15555550100@192.0.2.10>;tag=a'], replayed: ["<sip:+15555550100@192.0.2.10>;tag=b"] })
    }],
    visible: [{
      defect: "the display name dropped on a 2xx to the INVITE: not an in-dialog request",
      leftTo: undefined,
      probe: headerProbe({
        name: "From",
        scope: on200,
        captured: ['"Alice" <sip:+15555550100@192.0.2.10>;tag=a'],
        replayed: ["<sip:+15555550100@192.0.2.10>;tag=b"]
      })
    }]
  },
  {
    id: "caller-dialog-address-kept",
    named: [
      {
        probe: towardCaller("To", "<sip:+15555550100@atlanta.example.com>;tag=a", "<sip:192.0.2.4>;tag=b"),
        facts: callerSent({ from: "<sip:192.0.2.4>;tag=b" }, "s13")
      },
      {
        probe: towardCaller("From", "<sip:+15555550101@atlanta.example.com>;tag=c", "<sip:+15555550101@192.0.2.20>;tag=d"),
        facts: callerSent({ from: "<sip:192.0.2.4>;tag=b", to: "<sip:+15555550101@192.0.2.20>" }, "s13")
      }
    ],
    visible: [
      {
        defect: "her INVITE's URI, a header parameter of her address dropped: not her address whole",
        leftTo: undefined,
        probe: towardCaller("To", "<sip:+15555550100@atlanta.example.com>;tag=a", "<sip:192.0.2.4>;tag=b"),
        facts: callerSent({ from: "<sip:192.0.2.4>;x-note=1;tag=b" }, "s13")
      },
      {
        defect: "a From toward the caller that is not the address her INVITE was sent to",
        leftTo: undefined,
        probe: towardCaller("From", "<sip:+15555550101@atlanta.example.com>;tag=c", "<sip:+15555550101@192.0.2.20>;tag=d"),
        facts: callerSent({ from: "<sip:192.0.2.4>;tag=b", to: "<sip:+15555550199@192.0.2.20>" }, "s13")
      }
    ]
  },
  {
    id: "response-identity-copied-from-request",
    named: [{
      probe: headerProbe({
        name: "From",
        step: "s2",
        scope: answering(100, "INVITE"),
        captured: ['"Alice A. Liddell" <sip:+15555550101@atlanta.example.com>;tag=c'],
        replayed: ['"Alice" <sip:+15555550101@atlanta.example.com>;tag=d'],
        leg: { role: "caller", direction: "to-caller" }
      }),
      facts: callerSent({ from: '"Alice" <sip:+15555550101@atlanta.example.com>;tag=d' }, "s2")
    }],
    visible: [{
      defect: "a response From toward the caller that is not the From her INVITE carried",
      leftTo: undefined,
      probe: headerProbe({
        name: "From",
        step: "s2",
        scope: answering(100, "INVITE"),
        captured: ['"Alice A. Liddell" <sip:+15555550101@atlanta.example.com>;tag=c'],
        replayed: ['"Al" <sip:+15555550101@atlanta.example.com>;tag=d'],
        leg: { role: "caller", direction: "to-caller" }
      }),
      facts: callerSent({ from: '"Alice" <sip:+15555550101@atlanta.example.com>;tag=d' }, "s2")
    }]
  },
  {
    id: "sdp-origin-stack-owned",
    named: [{ probe: mintedOrigin(true) }],
    visible: [{ defect: "the same numbers-only row the confrontation did not read as a minted origin", leftTo: undefined, probe: mintedOrigin(false) }]
  },
  {
    id: "sdp-session-id-restated",
    named: [{ probe: restatedOrigin("o=- 111 105 IN IP4 192.0.2.10"), facts: sessionOpenedAt8 }],
    visible: [
      {
        defect: "a sess-id that is not the one the leg's session opened with: the replay minted it",
        leftTo: undefined,
        probe: restatedOrigin("o=- 333 105 IN IP4 192.0.2.10"),
        facts: sessionOpenedAt8
      },
      {
        defect: "the restated description out of RFC 4566 §5 line order (a= ahead of c=): its `order` row",
        leftTo: undefined,
        probe: bodyProbe({
          mediaType: "application/sdp",
          scope: onAck,
          compare: "sdp",
          step: "s34",
          captured: ["m=audio 4000 RTP/AVP 8", "c=IN IP4 192.0.2.10", "a=sendrecv"],
          replayed: ["m=audio 4000 RTP/AVP 8", "a=sendrecv", "c=IN IP4 192.0.2.10"],
          sdp: { section: "m0", line: "order" }
        }),
        facts: sessionOpenedAt8
      }
    ]
  },
  {
    id: "sdp-version-step-one",
    named: [{ probe: steppedOrigin({ versionStepOne: true }) }],
    visible: [
      {
        defect: "the same gap the confrontation did not read as a step of one: the replay jumped too, or stands above the capture",
        leftTo: undefined,
        probe: steppedOrigin({})
      }
    ]
  }
]

const SENDER_OWNED: ReadonlyArray<RfcFixture> = [
  {
    id: "refer-progress-notify-state",
    named: [{
      probe: headerProbe({ name: "Subscription-State", step: "s11", scope: inDialog("NOTIFY"), captured: ["terminated"], replayed: ["pending"] }),
      facts: factsWith({ referOutcomeAtStep: new Map([["s11", 603]]) })
    }],
    visible: [{
      defect: "a progress state on a NOTIFY the run shows no terminal NOTIFY following",
      leftTo: undefined,
      probe: headerProbe({ name: "Subscription-State", step: "s11", scope: inDialog("NOTIFY"), captured: ["terminated"], replayed: ["pending"] })
    }]
  },
  {
    id: "release-cause-value-not-compared",
    named: [{ probe: headerProbe({ name: "Reason", scope: on486, captured: ["Q.850;cause=16"], replayed: ["Q.850 ;cause=16"] }) }],
    visible: [{ defect: "another cause", leftTo: undefined, probe: headerProbe({ name: "Reason", scope: on486, captured: ["Q.850;cause=16"], replayed: ["Q.850;cause=31"] }) }]
  },
  {
    id: "session-refresh-interval-withheld",
    named: [{ probe: headerProbe({ name: "Session-Expires", inbound: true, captured: ["1800"] }) }],
    visible: [{
      defect: "an interval the replay states where the capture stated another",
      leftTo: undefined,
      probe: headerProbe({ name: "Session-Expires", inbound: true, captured: ["1800"], replayed: ["0"] })
    }]
  },
  {
    id: "charging-vector-own-reinvite-toward-caller-varies",
    named: [{
      probe: headerProbe({
        name: "P-Charging-Vector",
        scope: inDialog("INVITE"),
        captured: ["icid-value=p"],
        replayed: ["icid-value=b"],
        inbound: true,
        inboundValues: ["icid-value=b"],
        leg: { role: "caller", direction: "to-caller" }
      })
    }],
    visible: [{
      defect: "a vector no peer handed in, on a re-INVITE toward the callee",
      leftTo: undefined,
      probe: headerProbe({
        name: "P-Charging-Vector",
        scope: inDialog("INVITE"),
        captured: ["icid-value=c"],
        replayed: ["icid-value=b"],
        inbound: true,
        inboundValues: ["icid-value=c"],
        leg: { role: "dialled", direction: "to-callee" }
      })
    }]
  },
  {
    id: "timestamp-restated",
    named: [{ probe: headerProbe({ name: "Timestamp", scope: onBye, captured: ["1789651651"], replayed: ["113.010"] }) }],
    visible: [{
      defect: "a delay part on a request: an echo's, never a request's own reading",
      leftTo: undefined,
      probe: headerProbe({ name: "Timestamp", scope: onBye, captured: ["1789651651"], replayed: ["113.010 0.5"] })
    }]
  },
  {
    id: "response-timestamp-echoed",
    named: [{ probe: headerProbe({ name: "Timestamp", scope: on200, replayed: ["1234"] }) }],
    visible: [{ defect: "a Timestamp the replay states on a request", leftTo: undefined, probe: headerProbe({ name: "Timestamp", replayed: ["1234"] }) }]
  },
  {
    id: "reason-stated-on-cancel",
    named: [{
      probe: headerProbe({
        name: "Reason",
        scope: { kind: "request", method: "CANCEL", inDialog: false },
        captured: ["Q.850;cause=16"],
        replayed: ["Q.850;cause=16", 'SIP;cause=487;text="Request Terminated"']
      })
    }],
    visible: [{
      defect: "a CANCEL whose Reason lost the captured cause",
      leftTo: undefined,
      probe: headerProbe({
        name: "Reason",
        scope: { kind: "request", method: "CANCEL", inDialog: false },
        captured: ["Q.850;cause=16"],
        replayed: ['SIP;cause=487;text="Request Terminated"']
      })
    }]
  }
]

const CAPABILITY: ReadonlyArray<RfcFixture> = [
  {
    id: "capability-set-relayed-past-release",
    named: [{ probe: relayedAllow("s20"), facts: factsWith({ releasedLegSteps: new Set(["s20"]) }) }],
    visible: [{
      defect: "the peer's advertisement carried on a response before its leg released, where the capture stated none",
      leftTo: "capability-set-added-by-stack",
      probe: relayedAllow("s17"),
      facts: factsWith({ releasedLegSteps: new Set(["s20"]) })
    }]
  },
  {
    id: "method-not-allowed-states-allow",
    named: [{ probe: headerProbe({ name: "Allow", scope: answering(405, "INVITE"), replayed: [RELAYED_SET] }) }],
    visible: [{
      defect: "an Allow the replay states on a 403, which RFC 3261 §8.2.1 does not ask for",
      leftTo: "capability-set-added-by-stack",
      probe: headerProbe({ name: "Allow", scope: answering(403, "INVITE"), replayed: [RELAYED_SET] })
    }]
  },
  {
    id: "failure-final-allow-events-relayed",
    named: [{
      probe: headerProbe({ name: "Allow-Events", scope: answering(404, "INVITE"), replayed: ["presence"], inbound: true, inboundValues: ["presence"] })
    }],
    visible: [{
      defect: "an Allow-Events no peer handed in",
      leftTo: "capability-set-added-by-stack",
      probe: headerProbe({ name: "Allow-Events", scope: answering(404, "INVITE"), replayed: ["presence"] })
    }]
  },
  {
    id: "capability-set-added-by-stack",
    named: [{ probe: headerProbe({ name: "Allow", captured: ["INVITE, ACK"], replayed: ["INVITE, ACK, BYE"] }) }],
    visible: [{ defect: "a set narrowed as well as widened", leftTo: "capability-set-not-generated", probe: headerProbe({ name: "Allow", captured: ["INVITE, ACK"], replayed: ["INVITE, BYE"] }) }]
  },
  {
    id: "locally-answered-non-invite-advertisement",
    named: [{ probe: headerProbe({ name: "Accept", scope: on200Bye, captured: ["application/sdp"] }) }],
    visible: [{
      defect: "an Accept the replay left off a 2xx to the INVITE, where RFC 3261 §20 Table 2 lets it stand",
      leftTo: "capability-set-not-generated",
      probe: headerProbe({ name: "Accept", scope: on200, captured: ["application/sdp"] })
    }]
  },
  {
    id: "capability-set-inert-on-transaction-derived",
    named: [{ probe: dropped("Allow", "INVITE, ACK, BYE", onAck) }],
    visible: [{
      defect: "an Allow-Events handed in on an ACK and dropped: outside RFC 3261 §20 Tables 2 and 3",
      leftTo: undefined,
      probe: dropped("Allow-Events", "presence", onAck)
    }]
  },
  {
    id: "capability-set-not-generated",
    named: [{ probe: minted("Allow", "INVITE, ACK, BYE") }],
    visible: [{ defect: "tokens the system was handed and the run drove in, dropped", leftTo: undefined, probe: dropped("Allow", "INVITE, ACK, BYE") }]
  }
]

const CAPTURED_ONLY: ReadonlyArray<RfcFixture> = [
  {
    id: "self-date-regenerated",
    named: [{
      probe: headerProbe({
        name: "Date",
        captured: ["Mon, 01 Jan 2024 00:00:00 GMT"],
        inbound: true,
        inboundValues: ["Mon, 01 Jan 2024 00:00:00 GMT"]
      })
    }],
    visible: [{
      defect: "a Date both sides carry: not one the replay left off",
      leftTo: undefined,
      probe: headerProbe({ name: "Date", captured: ["Mon, 01 Jan 2024 00:00:00 GMT"], replayed: ["Tue, 02 Jan 2024 00:00:00 GMT"] })
    }]
  },
  {
    id: "self-timestamp-regenerated",
    named: [{ probe: headerProbe({ name: "Timestamp", captured: ["1234"], inbound: true, inboundValues: ["1234"] }) }],
    visible: [{ defect: "a request's Timestamp dropped from a response the system relayed", leftTo: undefined, probe: dropped("Timestamp", "1234", on200) }]
  },
  {
    id: "minted-final-timestamp-echoed",
    named: [{
      probe: headerProbe({ name: "Timestamp", scope: answering(403, "INVITE"), captured: ["1234"], inbound: true, inboundValues: ["1234"] })
    }],
    visible: [{ defect: "a request's Timestamp dropped from a 2xx the system relayed, its input driven in", leftTo: undefined, probe: dropped("Timestamp", "1234", on200) }]
  },
  {
    id: "minted-glare-rejection-relays-nothing",
    named: [{ probe: headerProbe({ name: "Subject", scope: on491, captured: ["call back"], inbound: true }) }],
    visible: [{
      defect: "a header a relayed 491 dropped, its input driven in",
      leftTo: undefined,
      probe: headerProbe({ name: "Subject", scope: on491, captured: ["call back"], inbound: true, driven: true })
    }]
  },
  {
    id: "glare-rejection-retry-after-not-generated",
    named: [{ probe: headerProbe({ name: "Retry-After", scope: on491, captured: ["2"] }) }],
    visible: [{ defect: "a peer's Retry-After a relayed 491 dropped, its input driven in", leftTo: undefined, probe: dropped("Retry-After", "2", on491) }]
  },
  {
    id: "access-network-context-not-generated",
    named: [{ probe: minted("P-Access-Network-Info", "3GPP-E-UTRAN-FDD") }],
    visible: [{ defect: "a P-Access-Network-Info the system was handed and the run drove in, dropped", leftTo: undefined, probe: dropped("P-Access-Network-Info", "3GPP-E-UTRAN-FDD") }]
  },
  {
    id: "identity-assertion-not-generated",
    named: [{ probe: minted("P-Asserted-Identity", ALICE) }],
    visible: [{ defect: "an assertion the system was handed and the run drove in, dropped", leftTo: undefined, probe: dropped("P-Asserted-Identity", ALICE) }]
  },
  {
    id: "privacy-instruction-not-generated",
    named: [{ probe: minted("Privacy", "id") }],
    visible: [{ defect: "a Privacy the system was handed and the run drove in, dropped", leftTo: undefined, probe: dropped("Privacy", "id") }]
  },
  {
    id: "release-cause-not-generated",
    named: [{ probe: minted("Reason", "Q.850;cause=16", on486) }],
    visible: [{ defect: "a Reason the system was handed and the run drove in, dropped", leftTo: undefined, probe: dropped("Reason", "Q.850;cause=16", on486) }]
  },
  {
    id: "body-descriptor-dropped-with-its-body",
    named: [{
      probe: headerProbe({ name: "Content-Disposition", captured: ["session; handling=required"], inbound: true, driven: true, scope: on180, bodiless: true })
    }],
    visible: [{
      defect: "a descriptor dropped from a message that still carries a body",
      leftTo: undefined,
      probe: headerProbe({ name: "Content-Disposition", captured: ["session; handling=required"], inbound: true, driven: true, scope: on180, bodiless: false })
    }]
  },
  {
    id: "body-metadata-not-generated",
    named: [{ probe: minted("MIME-Version", "1.0") }],
    visible: [{ defect: "a MIME-Version the system was handed and the run drove in, dropped", leftTo: undefined, probe: dropped("MIME-Version", "1.0") }]
  },
  {
    id: "invitation-expiry-not-generated",
    named: [{ probe: minted("Expires", "120") }],
    visible: [{ defect: "an Expires the system was handed and the run drove in, dropped", leftTo: undefined, probe: dropped("Expires", "120") }]
  },
  {
    id: "relay-input-not-driven",
    named: [{ probe: headerProbe({ name: "Subject", scope: onBye, captured: ["call ended"], inbound: true, inboundValues: ["call ended"], driven: false }) }],
    visible: [{
      defect: "a lost item the system was never handed",
      leftTo: undefined,
      probe: headerProbe({ name: "Subject", scope: onBye, captured: ["call ended"], inbound: true, inboundValues: ["call over"], driven: false })
    }]
  }
]

const SHAPE: ReadonlyArray<RfcFixture> = [
  {
    id: "prack-automatic-stray",
    named: [{ probe: shape({ shape: "extra-message", method: "PRACK" }) }],
    visible: [{ defect: "an UPDATE the flow never scripted", leftTo: undefined, probe: shape({ shape: "extra-message", method: "UPDATE" }) }]
  },
  {
    id: "bye-teardown-cross-stray",
    named: [{ probe: shape({ shape: "serviced-stray", method: "BYE", action: "auto-reacted" }) }],
    visible: [{ defect: "a BYE the system sent that no endpoint answered on its own", leftTo: undefined, probe: shape({ shape: "extra-message", method: "BYE" }) }]
  },
  {
    id: "ack-copy-relayed-once",
    named: [{ probe: shape({ shape: "repeated-request", method: "ACK", copies: 1 }) }],
    visible: [
      { defect: "a second extra copy of the same ACK", leftTo: undefined, probe: shape({ shape: "repeated-request", method: "ACK", copies: 2 }) }
    ]
  },
  {
    id: "trying-on-non-invite-not-reproduced",
    named: [
      {
        probe: shape({ shape: "status-substitution", captured: 100, observed: 481 }, answering(481, "BYE"), "s22"),
        facts: factsWith({ tryingOnNonInviteSteps: new Map([["s22", 481]]) })
      },
      {
        probe: shape({ shape: "status-substitution", captured: 100, observed: 200 }, answering(200, "CANCEL"), "s12"),
        facts: factsWith({ tryingOnNonInviteSteps: new Map([["s12", 200]]) })
      },
      {
        probe: shape({ shape: "status-substitution", captured: 100, observed: 200 }, on200Bye, "s22"),
        facts: factsWith({ tryingOnNonInviteSteps: new Map([["s22", 481]]), tryingOnEarlyByeSteps: new Set(["s22"]) })
      }
    ],
    visible: [
      {
        defect: "a 200 to a BYE sent after the INVITE's final ended its early dialog",
        leftTo: undefined,
        probe: shape({ shape: "status-substitution", captured: 100, observed: 200 }, on200Bye, "s22"),
        facts: factsWith({ tryingOnNonInviteSteps: new Map([["s22", 481]]) })
      },
      {
        defect: "a 481 to a CANCEL, which RFC 3261 §9.2 answers 200",
        leftTo: undefined,
        probe: shape({ shape: "status-substitution", captured: 100, observed: 481 }, answering(481, "CANCEL"), "s12"),
        facts: factsWith({ tryingOnNonInviteSteps: new Map([["s12", 200]]) })
      }
    ]
  },
  {
    id: "bye-not-relayed-onto-cancelled-early-dialog",
    named: [{ probe: shape({ shape: "missing-message", method: "BYE" }, undefined, "s16"), facts: factsWith({ byeBesideCancelSteps: new Set(["s16"]) }) }],
    visible: [{
      defect: "a BYE missing on a dialog the document shows no CANCEL and BYE of the caller beside",
      leftTo: undefined,
      probe: shape({ shape: "missing-message", method: "BYE" }, undefined, "s16")
    }]
  },
  {
    id: "cancelled-call-not-dialled",
    named: [{ probe: shape({ shape: "missing-message", method: "INVITE" }, undefined, "s4"), facts: factsWith({ dialledAfterCancelSteps: new Set(["s4"]) }) }],
    visible: [{
      defect: "a CANCEL missing on the leg the capture dialled after the caller's CANCEL: the replay dialled it all the same",
      leftTo: undefined,
      probe: shape({ shape: "missing-message", method: "CANCEL" }, undefined, "s4"),
      facts: factsWith({ dialledAfterCancelSteps: new Set(["s4"]) })
    }]
  },
  {
    id: "cancel-past-answer-not-relayed",
    named: [
      {
        probe: shape({ shape: "method-substitution", expected: "CANCEL", observed: "ACK" }, undefined, "s13"),
        facts: factsWith({ cancelPastAnswerSteps: new Set(["s13"]) })
      },
      {
        probe: shape({ shape: "missing-message", method: "CANCEL" }, undefined, "s13"),
        facts: factsWith({ cancelPastAnswerSteps: new Set(["s13"]) })
      }
    ],
    visible: [
      {
        defect: "an INVITE where the capture relayed a CANCEL past the answer: no ACK or BYE follows it",
        leftTo: undefined,
        probe: shape({ shape: "method-substitution", expected: "CANCEL", observed: "INVITE" }, undefined, "s13"),
        facts: factsWith({ cancelPastAnswerSteps: new Set(["s13"]) })
      },
      {
        defect: "a CANCEL missing toward a callee that had not answered when it left",
        leftTo: undefined,
        probe: shape({ shape: "missing-message", method: "CANCEL" }, undefined, "s13")
      }
    ]
  },
  {
    id: "answer-relayed-ahead-of-crossing-cancel",
    named: [{
      probe: shape({ shape: "status-substitution", captured: 487, observed: 200 }, { kind: "initial-invite" }, "s14"),
      facts: factsWith({ cancelCrossingAnswerSteps: new Set(["s14"]) })
    }],
    visible: [{
      defect: "a 2xx in place of the caller's 487 where no callee answered before her CANCEL",
      leftTo: undefined,
      probe: shape({ shape: "status-substitution", captured: 487, observed: 200 }, { kind: "initial-invite" }, "s14")
    }]
  },
  {
    id: "refer-progress-notify-report",
    named: [{ probe: referReport, facts: factsWith({ referOutcomeAtStep: new Map([["s11", 603]]) }) }],
    visible: [{
      defect: "a terminal NOTIFY of the replay reporting another outcome than the capture's",
      leftTo: undefined,
      probe: referReport,
      facts: factsWith({ referOutcomeAtStep: new Map([["s11", 486]]) })
    }]
  },
  {
    id: "refer-progress-terminal-notify",
    named: [
      { probe: strayNotify, facts: referredOn() },
      { probe: notifyForBye("s15"), facts: referredOn() }
    ],
    visible: [
      {
        defect: "a NOTIFY the flow never scripted on a document with no REFER outcome",
        leftTo: undefined,
        probe: strayNotify
      },
      {
        defect: "a second NOTIFY past the REFER's terminal one",
        leftTo: undefined,
        probe: strayNotify,
        facts: referredOn({ terminals: 2 })
      },
      {
        defect: "a NOTIFY where BYE was due on another leg than the REFER's",
        leftTo: undefined,
        probe: notifyForBye("s15"),
        facts: referredOn({ byeLeg: "B" })
      },
      {
        defect: "a NOTIFY where BYE was due before the REFER's progress NOTIFY",
        leftTo: undefined,
        probe: notifyForBye("s9"),
        facts: referredOn()
      },
      {
        defect: "a NOTIFY no step claimed on a leg beside the REFER's terminal one",
        leftTo: undefined,
        probe: strayNotify,
        facts: referredOn({ elsewhere: true })
      }
    ]
  },
  {
    id: "trying-provisional-unscripted",
    named: [{
      probe: shape({ shape: "status-substitution", captured: 200, observed: 100 }, answering(100, "INVITE"), "s14"),
      facts: factsWith({ untriedInviteSteps: new Set(["s14"]) })
    }],
    visible: [{
      defect: "a 100 on a transaction the document scripts one for",
      leftTo: undefined,
      probe: shape({ shape: "status-substitution", captured: 200, observed: 100 }, answering(100, "INVITE"), "s22"),
      facts: factsWith({ untriedInviteSteps: new Set(["s14"]) })
    }]
  },
  {
    id: "trying-omitted-before-final",
    named: [{ probe: shape({ shape: "trying-omitted", final: 487, afterMs: 2 }, answering(100, "INVITE"), "s13") }],
    visible: [{
      defect: "a final past the 200 ms the server transaction had to send its 100 in",
      leftTo: undefined,
      probe: shape({ shape: "trying-omitted", final: 487, afterMs: 250 }, answering(100, "INVITE"), "s13")
    }]
  },
  {
    id: "request-past-release-answered-481",
    named: [{
      probe: shape({ shape: "status-substitution", captured: 501, observed: 481 }, answering(481, "NOTIFY"), "s28"),
      facts: factsWith({ roundsPastReleaseSteps: new Set(["s28"]) })
    }],
    visible: [{
      defect: "a 481 to a round the peer opened before its leg's release",
      leftTo: undefined,
      probe: shape({ shape: "status-substitution", captured: 501, observed: 481 }, answering(481, "NOTIFY"), "s23"),
      facts: factsWith({ roundsPastReleaseSteps: new Set(["s28"]) })
    }]
  }
]

/** One fixture per generic row, in `RFC_ROWS` order. */
export const RFC_FIXTURES: ReadonlyArray<RfcFixture> = [
  ...KEEPALIVE,
  ...STACK_OWNED,
  ...SENDER_OWNED,
  ...CAPABILITY,
  ...CAPTURED_ONLY,
  ...SHAPE
]
