# B2BUA RFC Corner-Case Coverage Matrix

Built by reviewing the SIP RFCs the B2BUA depends on, enumerating the corner
cases where correct **back-to-back UA** behaviour is non-obvious, then
cross-referencing each against the existing automated test corpus.

A B2BUA = **UAS on the A-leg + UAC on the B-leg**, terminating two independent
transactions/dialogs and bridging them. "Regenerate" = mint a fresh value on the
outgoing leg; "absorb" = consume locally, do not forward. Most B2BUA-specific
bugs are *one leg's identity/numbering/SDP leaking onto the other leg*.

## RFCs in scope (by reference weight in the tree)

| RFC | Topic | B2BUA relevance |
|-----|-------|-----------------|
| 3261 | Core SIP | Transactions, dialogs, CANCEL, ACK, BYE, routing — foundation |
| 3262 | 100rel / PRACK | Reliable provisionals, per-leg RSeq/RAck |
| 3264 | Offer/Answer model | Per-leg O/A state machine, SDP bridging |
| 3311 | UPDATE | Early-dialog media change, refresh |
| 3515 / 5589 | REFER / call control | Blind & attended transfer |
| 3891 | Replaces | Attended transfer dialog match |
| 6665 / 3265 | Events / SUBSCRIBE-NOTIFY | REFER implicit subscription |
| 3326 | Reason header | Cross-leg teardown cause propagation |
| 4028 | Session Timers | Session-Expires/Min-SE/refresh keepalive |
| 4566 | SDP | Body parsing/validation for bridging |
| 3550/3551/5761/5009/4733 | RTP/RTCP/mux/early-media/DTMF | Media bridging (signalling-only today) |
| 3581 | rport | Symmetric response routing |
| 3263 | Locating servers | DNS NAPTR/SRV B-leg failover |
| 3325/6442 | PAI / Geolocation | Identity trust-boundary handling |
| 4475 | Torture | Parser robustness |

## Legend

- ✅ **Covered** — an explicit test asserts this behaviour.
- 🟡 **Partial** — exercised implicitly, at a different layer (e.g. `sip-txn`
  rather than dual-leg B2BUA), or only one side of the case is tested.
- ❌ **Gap** — no test; add one.

Severity = operational risk if the behaviour is wrong.

---

## RFC 3261 — Core (transactions, ACK, CANCEL, dialogs, routing)

### Transaction state machines & retransmission

| ID | § | Scenario | Expected B2BUA behaviour | Sev | Status | Where / suggested test |
|----|---|----------|--------------------------|-----|--------|------------------------|
| INV-TXN-1 | 17.1.1 | Dup A-leg INVITE before answer | Server txn absorbs dup; no 2nd B-leg | high | 🟡 | sip-txn `duplicate_request_retransmits_cached_response`; add B2BUA `dup_invite_aleg_absorbed_no_second_bleg` |
| INV-TXN-2 | 17.1.1.2 | B-leg INVITE no response → Timer A | UAC retransmits doubling to T2 | high | ✅ | sip-txn `client_retransmits_on_timer_a_cadence` |
| INV-TXN-3 | 17.1.1.2 | B-leg INVITE → Timer B (64·T1) | Txn fails → A-leg 408 + teardown | high | 🟡 | sip-txn `timer_b_emits_timeout_event`; add B2BUA `bleg_timerB_yields_aleg_408_and_teardown` |
| INV-TXN-4 | 17.1.1.2 | Response stops Timer A | First response halts retransmit | med | ✅ | sip-txn `provisional_response_stops_retransmit` |
| INV-TXN-5 | 17.1.1.2 | Non-2xx → Timer D absorb | Completed absorbs retransmitted final | med | ✅ | sip-txn `non_2xx_invite_final_absorbs_retransmits_for_timer_d` |
| INV-100-1 | 17.2.1 | Slow first response on A-leg | Send 100 Trying locally (≤T1) | med | ✅ | sip-txn `absorbs_100_for_invite_cseq` |
| INV-100-2 | 8.1.1.7 | B-leg 100 Trying | Absorbed, not mapped to A-leg | low | 🟡 | proxy `absorbs_downstream_100`; add B2BUA `bleg_100_not_forwarded` |
| NIT-1 | 17.1.2.2 | B-leg BYE/OPTIONS no reply → Timer E | Non-INVITE retransmit to T2 | high | ✅ | sip-txn `non_invite_keeps_retransmitting_after_provisional` |
| NIT-2 | 17.1.2.2 | B-leg non-INVITE → Timer F | Give up request, still tear down | high | 🟡 | bye_no_200_reap covers wedged BYE; add `bleg_bye_timerF_timeout_still_tears_down` |
| NIT-3 | 17.2.2 | A-leg BYE retransmit → Timer J | Absorb; one B-leg BYE only | high | ✅ | sip-txn `cancel_txns_for_call_spares_server_timer_j_absorption` |
| RTX-4 | 17 | Terminal state releases all leg timers | CancelAll physically frees queue slots | high | ✅ | b2bua `cancel_physically_reclaims_the_queue_slot`, rules `invariants_append_cleanup_on_terminated` |
| RTX-5 | 13.3.1.4 | 2xx resend exhausted, no ACK | Give up → BYE, release both legs | high | ❌ | add `aleg_2xx_no_ack_full_teardown_both_legs` |

### ACK handling (the 2xx vs non-2xx split)

| ID | § | Scenario | Expected B2BUA behaviour | Sev | Status | Where / suggested test |
|----|---|----------|--------------------------|-----|--------|------------------------|
| ACK-1 | 17.1.1.3 | B-leg non-2xx → ACK | B2BUA generates ACK, same branch (part of INVITE txn) | high | ✅ | sip-txn `client_auto_acks_non_2xx_final` |
| ACK-2 | 13.2.2.4 | B-leg 2xx → ACK | New branch, separate txn, dialog target | high | 🟡 | sip-txn `ack_for_2xx_passes_through`, generators ACK-2xx; add B2BUA `bleg_2xx_ack_new_branch_separate_txn` |
| ACK-3 | 13.3.1.4 | A-leg 2xx ACK arrives | Absorb, stop 2xx resend, not forwarded | high | ✅ | sip-txn `ack_for_2xx_passes_through` + `self_release_fires...` |
| ACK-4 | 13.3.1.4 | A-leg 2xx, ACK never arrives | Retransmit 2xx then BYE | high | ❌ | add `aleg_2xx_no_ack_retransmit_then_bye` |
| ACK-5 | 13.2.2.4 | A-leg ACK carries SDP (delayed-offer answer) | Honour body, map to B-leg | med | ✅ | reinvite `alice_reinvite`, fake_prack `delayed_offer_fallback` |
| ACK-6 | 17.1.1.2 | Retransmit non-2xx after ACK | Resend same ACK (Timer D) | med | ✅ | sip-txn `non_2xx_invite_final_absorbs_retransmits_for_timer_d` |
| ACK-7 | 13.2.2.4 | Orphan/mismatched ACK | Absorb silently, no phantom dialog | med | 🟡 | sip-txn `ack_for_non_2xx_is_absorbed`; add explicit orphan-ACK case |

### CANCEL & 487

| ID | § | Scenario | Expected B2BUA behaviour | Sev | Status | Where / suggested test |
|----|---|----------|--------------------------|-----|--------|------------------------|
| CANCEL-1 | 9.1 | A-leg CANCEL while B-leg pending | 200 to CANCEL, 487 to A-INVITE, mint B-leg CANCEL | high | ✅ | sip-txn `cancel_sends_200_and_487_and_emits_cancelled` + cancel_during_slow_decision |
| CANCEL-2 | 9.2 | A-leg CANCEL after B-leg 2xx | Too late: no B-CANCEL, BYE the new leg | high | ❌ | add `cancel_after_2xx_no_bleg_cancel_bye_instead` |
| CANCEL-3 | 9.2 | CANCEL for unknown txn | 481, never forward | med | ✅ | sip-txn `unmatched_cancel_gets_481_and_emits_nothing` |
| CANCEL-4 | 9.1 | B-leg CANCEL before any 1xx | Defer CANCEL until provisional | high | ✅ | rfc-audit `cancelAfter1xx`; suppress_18x `failover_no_answer` |
| CANCEL-5 | 9.2 | CANCEL matching by branch | Matches INVITE server txn by top-Via | high | ✅ | proxy `cancel_after_a_minute...follows_the_invite`, generators CANCEL Via verbatim |
| CANCEL-6 | 9.1 | B-leg 2xx after B-leg CANCEL (glare) | 2xx wins: ACK then BYE, no leak | high | ❌ | add `bleg_cancel_glare_2xx_ack_then_bye` |
| CANCEL-7 | 9.1/16.10 | CANCEL Route echoes INVITE | Same Route set + B-leg branch | med | ✅ | rfc-audit `cancelRouteEchoesInvite` |
| 487-1 | 9.1 | A-INVITE terminated after CANCEL | 487, server txn drives Timer G/H | high | ✅ | sip-txn `cancel_sends_200_and_487...` |
| 487-3 | 9.1 | B-leg returns 487 | ACK it, map to A-leg | med | 🟡 | covered via failure relay; add explicit `bleg_487_acked_and_mapped` |

### re-INVITE / glare

| ID | § | Scenario | Expected B2BUA behaviour | Sev | Status | Where / suggested test |
|----|---|----------|--------------------------|-----|--------|------------------------|
| REINV-1 | 14.1 | re-INVITE forwarded per leg | Per-leg CSeq++/new branch, target preserved | high | ✅ | reinvite `alice_reinvite`/`bob_reinvite` |
| REINV-2 | 14.2 | Both legs re-INVITE (glare) | Incoming gets 491, no deadlock | high | ✅ | reinvite `crossing_reinvite_glare`; rfc-audit `concurrentReInvite500or491` |
| REINV-3 | 14.1 | B-leg returns 491 | Back off + retry, no instant loop | med | ❌ | add `bleg_491_backoff_before_retry` |
| REINV-4 | 14.1 | Failed re-INVITE (4xx/5xx) | Keep prior session, don't drop call | high | 🟡 | fake_prack `update_codec_mismatch` (UPDATE); add re-INVITE `failed_reinvite_keeps_dialog_state` |

### BYE & in-dialog routing

| ID | § | Scenario | Expected B2BUA behaviour | Sev | Status | Where / suggested test |
|----|---|----------|--------------------------|-----|--------|------------------------|
| BYE-1 | 15.1 | A-leg BYE | 200 to A, mint B-leg BYE, tear both | high | ✅ | basic_call, rules `in_dialog_bye_selects_relay_bye` |
| BYE-2 | 15.1.2 | BYE unknown dialog | 481 | high | ✅ | orphan_reject_no_leak; rfc-audit `unknownDialog481` |
| BYE-3 | 15 | BYE on early dialog | Use CANCEL not BYE | med | ✅ | rfc-audit `noByeOutsideOrEarlyDialog` |
| BYE-4 | 12.2 | Simultaneous double BYE | Idempotent teardown | high | 🟡 | reaper covers wedge; add `simultaneous_double_bye_idempotent` |
| BYE-5 | 12.2 | In-dialog request per-leg routing | Uses leg's remote target + route set | high | ✅ | b2bua `confirm_dialog_captures_b_leg_route_set...`, relay tests |
| BYE-6 | 12.2.1.1 | Per-leg CSeq monotonic | Each leg own CSeq counter | med | ✅ | rfc-audit `cseq` rules, generators |
| BYE-7 | 15.1.1 | BYE mid B-leg re-INVITE | Abort in-flight re-INVITE, BYE | med | 🟡 | refer a-bye-during-realign analogue; add `bye_during_reinvite_aborts_reinvite` |

### Per-leg identity / Via / Max-Forwards (the load-bearing B2BUA invariants)

| ID | § | Scenario | Expected B2BUA behaviour | Sev | Status | Where / suggested test |
|----|---|----------|--------------------------|-----|--------|------------------------|
| ID-1 | 12.1.1 | Independent tags per leg | A To-tag & B From-tag minted independently | high | 🟡 | implicit in basic_call; add explicit `tags_independent_per_leg` |
| ID-2 | 8.1.1.4 | New Call-ID on B-leg | Never reuse A-leg Call-ID | high | 🟡 | implicit in basic_call; add `bleg_new_callid` |
| ID-3 | 8.1.1.5 | Independent CSeq spaces | A CSeq must not leak to B | high | 🟡 | rfc-audit cseq per-dialog; add `cseq_independent_per_leg` |
| ID-4 | 8.1.1.2 | B-leg initial INVITE tagless To | No A To-tag copied into B initial | high | ✅ | rfc-audit `noToTagOnInitialRequest` |
| VIA-1 | 8.1.1.7 | Own Via/branch per leg | Fresh z9hG4bK per outgoing request | high | ✅ | sip-txn `branch_has_magic_cookie_and_is_unique`, stack_identity |
| VIA-3 | 8.1.1.5 | Max-Forwards reset to 70 | B2BUA originates, not decrements | med | 🟡 | proxy decrements; add B2BUA `bleg_maxforwards_reset_70` |
| HDR-2 | 20.5 | **Contact rewritten to B2BUA per leg** | Far-end Contact never passed through | high | 🟡 | implicit (in-dialog returns to B2BUA works); add explicit `contact_rewritten_to_b2bua_per_leg` |
| HDR-5 | 12.1.1 | Record-Route not cross-leg leaked | Each leg's RR is leg-local | high | ✅ | b2bua `confirm_dialog_*record_route*` |
| HDR-7 | 8.1.1.8 | Contact on all dialog-forming msgs | INVITE/2xx carry B2BUA Contact | high | ✅ | rfc-audit `contactOnInvite`, proxy_b2bua |

### Routing edge cases, redirects, header policy, auth

| ID | § | Scenario | Expected B2BUA behaviour | Sev | Status | Where / suggested test |
|----|---|----------|--------------------------|-----|--------|------------------------|
| RR-1 | 12/16.6 | B2BUA does NOT Record-Route | It's a UA, not a proxy | high | ✅ | rfc-audit `recordRouteOnlyOnDialogCreating` (proxy RRs, B2BUA doesn't) |
| RR-4 | 16.6 | Strict-route first-hop swap | Apply strict-route shuffle on send | med | ✅ | rfc-audit `strictRouteShuffleOnSend`/`strictRouteRewriteHandled`, generators |
| LOOP-1 | 16.3 | B-leg target loops to B2BUA (self) | App-level loop guard (Via blind due to new Call-ID) | med | ❌ | add `b2bua_self_loop_detected` |
| MERGE-1 | 8.2.2.2 | Forked dup A-INVITE | 482 Loop Detected on 2nd copy | med | ❌ | add `merged_invite_482_second_copy` |
| RDR-1 | 8.1.3.4 | B-leg returns 3xx | Follow redirect or map; don't leak B Contacts to A | high | 🟡 | numbering_plan B2BUA-emitted 302 only; add `bleg_3xx_followed_not_leaked_to_aleg` |
| RDR-2 | 8.1.3.4 | 3xx loop | Bounded redirect recursion | med | ❌ | add `redirect_recursion_bounded` |
| OOD-2 | 8.2.1 | Unknown method on a leg | 405 + Allow | med | ✅ | rfc-audit `unsupportedMethod405Allow` |
| HDR-1 | 8.2.2 | Unsupported Require/Proxy-Require | 420 + Unsupported, don't forward | high | ✅ | rfc-audit `unsupportedExtension420` |
| HDR-3 | 20.2 | Allow/Supported reflect B2BUA | Advertise own caps, not peer's | med | ❌ | add `capabilities_reflect_b2bua_not_peer` |
| AUTH-1 | 22.1 | B-leg 401/407 | Answer locally with creds or map; don't leak nonce | high | ❌ | add `bleg_401_answered_locally_with_creds` |
| AUTH-2 | 22.2 | B2BUA challenges A-leg | Validate credentialed retry before B-leg | med | ❌ | add `aleg_challenge_validated_before_bleg` |
| AUTH-3 | 22.3 | Auth retry CSeq++ | Increment per leg | high | ❌ | add `auth_retry_cseq_incremented` |
| AUTH-4 | 22.1 | Repeated 401 | Bounded retries | med | ❌ | add `auth_retry_bounded` |

---

## RFC 3262 / 3264 / 3311 — Offer/Answer, 100rel, UPDATE

### Offer/answer placement & legality

| ID | § | Scenario | Expected B2BUA behaviour | Sev | Status | Where / suggested test |
|----|---|----------|--------------------------|-----|--------|------------------------|
| OA-PLACE-1 | 3264 §5 | Offer in INVITE / answer in 2xx | Own O/A per leg | high | ✅ | basic_call, sdp_answer |
| OA-PLACE-2 | 3264 §5 | Delayed offer (offer in 2xx / answer in ACK) | Form 2xx offer, consume ACK answer | high | ✅ | reinvite `alice_reinvite`, fake_prack `delayed_offer_fallback`, suppress_18x `failover_no_answer` |
| OA-PLACE-3 | 3262 §5 | Offerless INVITE + reliable 1xx → offer in rel-1xx | First reliable provisional carries offer | high | ❌ | add `offerless_invite_offer_in_rel1xx` |
| OA-PLACE-4 | 3262 §5 | Offer in reliable 1xx → answer in PRACK | Answer rides PRACK | high | ✅ | prack, fake_prack; rfc-audit `prackOfferAnswerModel` |
| OA-PLACE-5 | 3262 §5 | Offer in PRACK → answer in 2xx-of-PRACK | Answer in PRACK 2xx | high | 🟡 | rfc-audit `prackResponseSemantics`; add explicit scenario |
| OA-ANSWER-UNREL-1 | 3262 §5 | Answer in unreliable 18x | Forbidden; only reliable carriers | high | 🟡 | rfc-audit reliability rules; add negative test |
| OA-NO-PENDING-1 | 3264 §4 | New offer while one outstanding (same leg) | Queue until answered | high | ✅ (advisory) | rfc-audit `noNewOfferWhileOfferPending` |
| OA-EMPTY-2XX-1 | 3264 §5 | 2xx to offered INVITE has empty body | Must answer | high | 🟡 | sdp_answer NoAliceSdp; add wire-level assert |

### 100rel negotiation & PRACK mechanics

| ID | § | Scenario | Expected B2BUA behaviour | Sev | Status | Where / suggested test |
|----|---|----------|--------------------------|-----|--------|------------------------|
| REL-NEG-1 | 3262 §3 | `Require:100rel` on INVITE | Reliable 1xx or 420 | high | ✅ | rfc-audit `requireReliable1xxOnRequire` |
| REL-NEG-3 | 3262 §4 | 100rel chosen on one leg only | Drive PRACK per leg, asymmetric ok | high | ✅ | fake_prack/suppress_18x (B2BUA PRACKs bob, downgrades A) |
| REL-NEG-4 | 3262 §3 | 100 Trying must not be reliable | Never stamp 100rel on 100 | med | ✅ | rfc-audit `reliable1xxHeaders` |
| RSEQ-MONO-1 | 3262 §3 | RSeq +1 per leg, own space | Per-leg RSeq increment | high | ✅ | rfc-audit `rseqMonotonic` |
| RSEQ-MAP-1 | 3262 §3/4 | RSeq re-minted across legs | A-leg RSeq unrelated to B-leg | high | ❌ | add cross-leg `rseq_reminted_across_legs` (audit is per-slice blind) |
| RACK-MAP-1 | 3262 §7.2 | RAck triple translated across legs | Per-leg (RSeq,CSeq,INVITE) | high | 🟡 | prack_forking rewrites RAck CSeq; add explicit cross-leg assert |
| PRACK-RESP-1 | 3262 §3 | PRACK match→2xx, no match→481 | UAS semantics | high | ✅ | rfc-audit `prackResponseSemantics` |
| PRACK-SERIAL-1 | 3262 §3 | 2nd reliable 1xx before 1st PRACKed | Serialize on PRACK | high | ✅ | rfc-audit `serialReliable1xx` |
| PRACK-RTX-1 | 3262 §4 | Dup reliable 1xx (same RSeq) | No duplicate PRACK | med | ✅ | rfc-audit `uacRseqStrictness` |
| PRACK-RTX-2 | 3262 §3 | Retransmitted PRACK after 2xx | Resend cached 2xx, no re-process | med | ❌ | add `prack_rtx_absorbed` |
| PRACK-LATE-1 | 3262 §3 | PRACK after INVITE final | Still 2xx it | med | ✅ | rfc-audit `prackAcceptedAfterFinal` |
| DELAY-2XX-1 | 3262 §3 | Un-PRACKed reliable 1xx with SDP | Delay 2xx until PRACK lands | high | ✅ | rfc-audit `delay2xxOnUnackedReliable1xxWithSdp` |

### Forking & early media

| ID | § | Scenario | Expected B2BUA behaviour | Sev | Status | Where / suggested test |
|----|---|----------|--------------------------|-----|--------|------------------------|
| PRACK-FORK-1 | 3262 §3 | Multiple early dialogs, per-dialog RSeq | Track RSeq per remote tag, PRACK each | high | ✅ | prack_forking `two_early_dialogs` |
| PRACK-FORK-3 | 3262 §4 | RSeq per-dialog not global | RSeq=1 from fork B in-order despite fork A | high | 🟡 | prack_forking covers two dialogs; add explicit concurrent-RSeq assert |
| PRACK-FORK-4 | 3264 §5 | Collapse to single A-leg O/A | One consistent answer to A | high | ✅ | prack_forking / prack_update_forking (answer on chosen fork) |
| EARLY-MEDIA-1 | 3262 §5 | Reliable 183+SDP early media | Bridge to A-leg before 200 | high | ✅ | promote_pem suite, fake_prack |
| EARLY-MEDIA-2 | 3264 §8 | Early→final SDP change | Re-anchor without dropping media | med | ✅ | promote_pem `resync_sdp_changed` |
| EARLY-MEDIA-4 | 3262 §5 | 183-SDP is the answer (not offer) | PRACK carries no body | high | ✅ | rfc-audit `prackOfferAnswerModel` |

### UPDATE (RFC 3311) & glare

| ID | § | Scenario | Expected B2BUA behaviour | Sev | Status | Where / suggested test |
|----|---|----------|--------------------------|-----|--------|------------------------|
| UPDATE-EARLY-1 | 3311 §5.1 | UPDATE in early dialog to change media | Process O/A, propagate to other leg | high | ✅ | prack_update_forking |
| UPDATE-ALLOWED-1 | 3311 §5 | UPDATE without peer `Allow: UPDATE` | Must not send; fall back to re-INVITE | high | ❌ | add `no_update_without_allow` |
| UPDATE-REJECT-1 | 3311 §5.2 | UPDATE offer can't be honoured | 488, session unchanged | high | ✅ | fake_prack `update_codec_mismatch` |
| UPDATE-OA-1 | 3311/3264 §8 | UPDATE O/A on established dialog | Full §8 rules, re-anchor | high | 🟡 | fake_prack `update_happy` (early); add established-dialog UPDATE |
| UPDATE-PENDING-1 | 3311/3264 §4 | UPDATE while O/A outstanding | 491 Request Pending | high | ❌ | add `update_491_when_oa_pending` |
| GLARE-UPDATE-1 | 3311/3261 §14.2 | Both ends UPDATE/re-INVITE | Loser 491 + backoff, no deadlock | high | 🟡 | reinvite glare covers re-INVITE; add UPDATE-glare |

### SDP bridging (RFC 3264 §6/§8, RFC 4566)

| ID | § | Scenario | Expected B2BUA behaviour | Sev | Status | Where / suggested test |
|----|---|----------|--------------------------|-----|--------|------------------------|
| SDP-HOLD-1 | 3264 §6.1 | `a=sendonly` hold | Answer recvonly/inactive, translate cross-leg | high | ✅ | media sdp_negotiation, rfc-audit `directionPairValid` |
| SDP-HOLD-3 | 3264 §8.4 | `c=0.0.0.0` non-zero port | Recognize hold, not reject | med | ✅ | rfc-audit `c0PortNonZero`, media port-0 hold |
| SDP-MLINE-COUNT-1 | 3264 §6 | Answer m-line count = offer | Preserve count, port-0 rejects | high | ✅ | rfc-audit `answerMLineCountMatchesOffer`, media |
| SDP-MLINE-ORDER-1 | 3264 §8 | Re-offer m-line count monotonic | Don't drop slots | high | ✅ | rfc-audit `reOfferMLineCountMonotonic` |
| SDP-NOINTERSECT-1 | 3264 §6.1 | Empty codec intersection | Reject stream; all-reject → 488 | high | 🟡 | sdp_answer `no_overlap_is_no_common_codec` (unit); add call-level 488 |
| SDP-CANTBRIDGE-1 | 3264 §6 | Unbridgeable transport (e.g. SCTP) | Reject/488, never relay dead media | high | ❌ | add `unbridgeable_media_488` |
| SDP-PT-STABLE-1 | 3264 §8.3.2 | Re-offer rebinds dynamic PT | PT→codec map stable for session | med | ✅ | rfc-audit `payloadTypeMappingStable` |
| SDP-ORIGIN-1 | 3264 §8 | `o=` continuity per leg | Same username/sess-id, version+1 | med | ✅ (advisory) | rfc-audit `sdpOriginContinuity`, sdp_diff |
| SDP-PARSE-1 | 3264 §5 | Malformed SDP | 488, don't relay invalid | med | ✅ | rfc-audit `sdpBodyParseable`, parser torture |

---

## RFC 3515 / 5589 / 3891 / 6665 / 3326 — Transfer & events

### REFER acceptance & scoping

| ID | § | Scenario | Expected B2BUA behaviour | Sev | Status | Where / suggested test |
|----|---|----------|--------------------------|-----|--------|------------------------|
| REFER-ACCEPT-1 | 3515 §2.4.2 | In-dialog REFER on bridged B-leg | 202 + immediate NOTIFY | high | ✅ | refer_allow `refer_allow_happy` |
| REFER-ACCEPT-2 | 3515 §3 | Zero or ≥2 Refer-To | 400 Bad Request | high | ❌ | add `refer_zero_or_two_refer_to_400` |
| REFER-ACCEPT-3 | 3515 §2 | REFER missing Contact | 400 | med | 🟡 | parser-level only; add end-to-end |
| REFER-SCOPE-1 | 5589 §6 | Out-of-dialog REFER | 481 (don't create call) | high | ✅ | refer_reject `refer_out_of_dialog` |
| REFER-SCOPE-2 | 3515 §2.4.1 | REFER on A-leg | 501 | med | ✅ | refer_reject (a-leg reject) |
| REFER-SCOPE-3 | 3515 §2 | REFER on early/unconfirmed B-leg | Don't arm transfer; reject | med | ❌ | add `refer_on_early_b_leg_rejected` |
| REFER-SCOPE-4 | 3515 §2 | Retransmitted REFER (same CSeq) | Absorb, resend 202, no 2nd subscription | high | ❌ | add `refer_retransmit_absorbed` |

### Implicit subscription & NOTIFY

| ID | § | Scenario | Expected B2BUA behaviour | Sev | Status | Where / suggested test |
|----|---|----------|--------------------------|-----|--------|------------------------|
| REFER-NOTIFY-1 | 3515 §2.4.4 | Immediate NOTIFY after 202 | active;expires, sipfrag 100 Trying | high | ✅ | refer_allow, refer_reject |
| REFER-NOTIFY-3 | 3515 §2.4.4 | C-leg 1xx → NOTIFY, dedupe repeats | active + 1xx sipfrag, dedupe | med | ✅ | refer_allow `c_multiple_18x` |
| REFER-NOTIFY-4 | 3515 §2.4.7 | Transfer success | terminated;reason=noresource, 200 sipfrag | high | ✅ | refer_full_transfer |
| REFER-NOTIFY-5 | 3515 §2.4.7 | Transfer target fails | terminated + real failure sipfrag | high | ✅ | refer_allow `c486`/`c603` |
| REFER-NOTIFY-6 | 6665 §4.1.3 | Subscription expiry (HTTP hung) | terminated;reason=timeout | high | ✅ | refer_reject `refer_http_timeout` |
| REFER-NOTIFY-7 | 6665 §4.1.3 | `expires` matches real timer | Advertised expires = actual lifetime | med | ❌ | hard-coded `active;expires=60`; add `notify_expires_matches_timer` |
| REFER-NOTIFY-8 | 6665 §4.4.1 | NOTIFY 481/408 from referrer | Stop subscription, no NOTIFY into void | med | ❌ | add `notify_481_stops_subscription` |
| REFER-NOTIFY-9 | 3265 §3.2.4 | Late C 1xx after terminal NOTIFY | No NOTIFY after terminated | med | ❌ | add `no_notify_after_terminated` |

### Refer-To, Replaces (attended), Refer-Sub

| ID | § | Scenario | Expected B2BUA behaviour | Sev | Status | Where / suggested test |
|----|---|----------|--------------------------|-----|--------|------------------------|
| REFER-TO-2 | 3515 §2.1 | Refer-To with embedded `?Replaces=` | Propagate Replaces to target INVITE | high | 🟡 | parsed (lazy_headers); currently dropped via 501 |
| REFER-TO-3 | 3515 §2.1 | Refer-To `;method=` non-INVITE | Honour/reject method param | low | ❌ | add `refer_to_method_param` |
| REPLACES-1 | 3891 §3 | Attended transfer (REFER+Replaces) | **Rejected 501 today** (scope gap) | high | ✅ (reject) | refer_reject `refer_replaces_rejected` → future honour-flow |
| REPLACES-2 | 3891 §3 | INVITE-with-Replaces targeting existing dialog | Match Call-ID+tags, replace; no match → 481 | high | ❌ | add `invite_replaces_match_and_no_match` |
| REPLACES-3 | 3891 §3 | Replaces early-only on confirmed dialog | 486 Busy Here | med | ❌ | add `invite_replaces_early_only_confirmed_486` |
| REPLACES-5 | 3891 §5 | Replaces unauthorized | 603 Declined | med | ❌ | add `invite_replaces_unauthorized_603` |
| REFER-ID-1 | 3515 §2.4.6 | 2nd REFER while transfer active | 491 (serialize, by design) | med | ✅ | refer_gating second-refer, refer_reject `second_during_authorizing` |
| REFER-SUB-1 | 4488 §3 | `Refer-Sub: false` | Honour (no NOTIFY) or echo in 202 | med | ❌ | add `refer_sub_false_no_notify` |
| REFERRED-BY-1 | 3892 | Referred-By present | Forward onto C INVITE | med | 🟡 | captured in code; add `referred_by_propagated_to_c` |

### Transfer concurrency, teardown, Reason, loops

| ID | § | Scenario | Expected B2BUA behaviour | Sev | Status | Where / suggested test |
|----|---|----------|--------------------------|-----|--------|------------------------|
| REFER-TXN-1 | 3261 §14 | A re-INVITE during transfer phases | 491 in realigning, relay earlier | high | ✅ | refer_gating `a_reinvite_*` |
| REFER-TXN-4 | 5589 | A BYE during a-realign | Begin-termination BYEs orphaned B+C | high | ✅ | refer_full_transfer `a_bye_during_a_realign` |
| REFER-TXN-5 | 3515 | C answers but realign fails/timeout | Rollback BYE all 3 + CDR rollback | high | ✅ | refer_c_realign / refer_full_transfer reject+timeout |
| REFER-TXN-6 | 6665 §4.1 | Overall watchdog fires | Cancel all transfer timers, terminate | high | ✅ | refer_timers `refer_overall_safety_fires` |
| REASON-1 | 3326 §2 | A/C BYE/CANCEL carries Reason | Propagate across legs | med | ❌ | add `bye_reason_propagated_across_legs` |
| REASON-2 | 3326 §2 | B2BUA-initiated rollback/Replaces BYE | Include Reason header | med | ❌ | add `transfer_rollback_bye_reason` |
| LOOP-1 | 5589 §6.1 | Refer-To target == self/A-leg | Detect & reject self-transfer | med | ❌ | add `refer_to_self_loop_rejected` |
| LOOP-2 | 3261 §16.3 | Chained transfer | Bound via Max-Forwards | low | ❌ | add `refer_chain_max_forwards` |

---

## RFC 4028 / RTP / 3581 / 3263 / 3325 / 6442 / 4475 — Session timers, media, transport, identity, torture

### RFC 4028 Session Timers — **whole area unimplemented (header parse only)**

| ID | § | Scenario | Expected B2BUA behaviour | Sev | Status | Where / suggested test |
|----|---|----------|--------------------------|-----|--------|------------------------|
| SESSTIMER-1 | §7.2/9 | One leg supports ST, other doesn't | Bridge: originate refresh on supporting leg | high | ❌ | add `sesstimer_bridges_when_far_leg_unsupported` |
| SESSTIMER-2 | §4/10 | Session-Expires below Min-SE | 422 + Min-SE | high | ❌ | add `sesstimer_422_below_min_se` |
| SESSTIMER-3 | §6 | B-leg returns 422 | Retry same INVITE, CSeq++, SE≥Min-SE; don't leak to A | high | ❌ | add `sesstimer_retry_on_422_from_bleg` |
| SESSTIMER-4 | §7.4 | refresher=uac/uas assignment | Track refresher duty per leg | high | ❌ | add `sesstimer_refresher_param_assignment` |
| SESSTIMER-5 | §10 | Missed refresh | Teardown timer → BYE both legs | high | ❌ | add `sesstimer_teardown_on_missed_refresh` |
| SESSTIMER-7 | §8.1 | Refresh-only re-INVITE/UPDATE | Don't re-bridge media, just reset timer + 200 | high | ❌ | add `sesstimer_refresh_no_media_rebridge` |
| SESSTIMER-10 | §10 | ST + OPTIONS keepalive both active | No double/conflicting teardown | high | ❌ | add `sesstimer_vs_options_keepalive_no_double_teardown` |
| SESSTIMER-11 | §9 | B2BUA is refresher | Actually send refresh on schedule | high | ❌ | add `sesstimer_b2bua_sends_own_refresh` |

> Note: **deliberate non-goal for now** (see Deliberate non-goals section). The
> B2BUA relies entirely on its own OPTIONS keepalive (300 s) for liveness. A
> strict ST peer would tear down long calls at Session-Expires while keepalive
> thinks the call is healthy — accepted risk until an ST peer is required.

### RTP / RTCP / mux / DTMF (B2BUA is signalling-only today; these are forward-looking for media-anchoring)

| ID | § | Scenario | Expected B2BUA behaviour | Sev | Status | Where / suggested test |
|----|---|----------|--------------------------|-----|--------|------------------------|
| RTP-1 | 3551 §6 | Same codec both legs | Passthrough, no PT rewrite | high | ✅ | media_e2e transparent relay |
| RTP-3 | 3264 §5.1 | Port-0 declined stream | Propagate decline, no RTP alloc | high | 🟡 | media port-0 hold (negotiation); add relay-side assert |
| RTP-4 | 5761 | rtcp-mux asymmetry across legs | De-mux on non-mux leg | high | ❌ | add `rtcp_mux_asymmetric_legs` |
| RTP-6 | 4733 | telephone-event DTMF | Passthrough event PT (rewrite per leg) | high | ❌ | add `rtp_telephone_event_passthrough` |
| RTP-8 | 3550 §11 | Symmetric RTP behind NAT | Latch to observed source | high | 🟡 | media latching partial; add explicit `rtp_symmetric_latching_behind_nat` |
| RTP-10 | 5009/3960 | Early media bridging | One-way per direction (P-Early-Media) | med | ✅ | media `P-Early-Media` gate, promote_pem |

### rport / DNS / identity / torture

| ID | § | Scenario | Expected B2BUA behaviour | Sev | Status | Where / suggested test |
|----|---|----------|--------------------------|-----|--------|------------------------|
| RPORT-1 | 3581 §4 | rport on inbound Via | UAS responds to received:rport, stamps it | high | 🟡 | proxy `...keeps_the_received_rport_target`; add B2BUA UAS-path `rport_uas_response_to_source_port` |
| LOCATE-2 | 3263 §4.3 | First SRV target times out | Failover to next target | high | 🟡 | numbering_plan reroute on failure (app-list); no DNS SRV. add `locate_failover_next_srv_on_timeout` |
| LOCATE-3 | 3263/16.7 | B-leg 503 | Try next target, honour Retry-After | high | ✅ | fake_prack/suppress_18x `failover` on 503 |
| IDENT-1 | 3325 §5 | PAI from trusted peer | Forward only within trust domain | high | ❌ | add `pai_forward_within_trust_domain` |
| IDENT-2 | 3325 §7 | `Privacy: id` + PAI, untrusted egress | Strip PAI, anonymize | high | ❌ | add `pai_stripped_on_privacy_id_untrusted` |
| IDENT-4 | 6442 §4 | Geolocation transparency | Forward unless Routing:no; keep cid: body | med | 🟡 | parsed (lazy_headers); add transparency behaviour test |
| TORTURE-1 | 4475 §3.1.1 | Valid weird grammar | Parse, don't 400 | high | ✅ | parser, compliance_matrix rfc4475-valid |
| TORTURE-2 | 4475 §3.1.2 | Malformed message | 400/drop, never panic | high | ✅ | parser, compliance_matrix rfc4475-invalid |
| TORTURE-4 | 4475 | Oversized header/URI | Bounded, 400/513 | high | ✅ | ADR-0007 long-value rejections |
| TORTURE-8 | 4475 | Bad in-dialog message on live call | Reject without tearing down healthy call | high | ❌ | add `torture_indialog_bad_msg_no_call_leak` |

---

## Coverage scorecard

| Area | Status | Notes |
|------|--------|-------|
| Transactions / retransmission / timers | ✅ Strong | sip-txn FSM (Timer A/B/D/E/J), b2bua epoch+Key driver |
| ACK (2xx vs non-2xx) | ✅ Strong | a few B2BUA-level explicit asserts missing |
| CANCEL / 487 | ✅ Strong | gap: CANCEL-after-2xx, CANCEL/2xx glare |
| re-INVITE / glare | ✅ Strong | gap: 491 backoff, failed-reINVITE-keeps-state |
| Offer/answer / SDP | ✅ Strong | gap: offerless-rel1xx, unbridgeable→488 |
| PRACK / 100rel | ✅ Strong | gap: cross-leg RSeq/RAck re-mint, PRACK rtx |
| UPDATE (3311) | 🟡 Moderate | gap: no-Allow guard, 491-pending, established-dialog |
| REFER / blind transfer | ✅ Very strong | best-covered area |
| Replaces / attended transfer | ⛔ Non-goal | parsed + **rejected 501**; deliberate, blind-only |
| NOTIFY / subscription | 🟡 Moderate | REFER-implicit only; expires-match & post-terminal gaps |
| **Session timers (4028)** | ⛔ Non-goal | unimplemented by decision; OPTIONS keepalive is liveness |
| Keepalive / OPTIONS | ✅ Very strong | B2BUA in-dialog + proxy health probe |
| BYE / teardown | ✅ Strong | gap: simultaneous double-BYE |
| Dialog routing / in-dialog | ✅ Strong | RR-reversed route set well tested |
| **Auth / challenge (401/407)** | ⛔ Non-goal | no Digest challenge/relay; deliberate for now |
| Route / Record-Route / Via | ✅ Strong | incl. double-RR, comma-fold, strict route |
| Redirect 3xx | ❌ Thin | only B2BUA-emitted 302; no UAC 3xx recursion |
| Error responses (4xx/5xx/6xx) | ✅ Strong | broad coverage |
| Parser / torture / 4475 | ✅ Very strong | + CVE + IPv6 + ABNF fuzz |
| Media / RTP | ✅ Strong | B2BUA signalling-only; mux/DTMF gaps if anchoring |
| Identity (PAI / Geolocation) | 🟡 Moderate | parsing strong; **trust-boundary stripping untested** |

## Deliberate non-goals (documented, not scheduled)

These areas are RFC-relevant but **intentionally out of scope for now** (per
project decision, 2026-06-14). They are recorded here so the gap is explicit and
the decision is auditable — they are *not* bugs and not on the test backlog until
reconsidered.

| Area | RFC | Current behaviour | Decision |
|------|-----|-------------------|----------|
| **Attended transfer / Replaces** | 3515 + 3891 | REFER carrying `Replaces=` is rejected **501**; no dialog-replacement flow | Not supported on purpose. Blind transfer only. |
| **Auth challenge / relay** | 3261 §22 | No 401/407 generation, no Digest challenge-response, no credentialed B-leg retry | Not supported on purpose for now. |
| **Session timers (Session-Expires / Min-SE)** | 4028 | Headers parse only; no negotiation, no 422, no refresh. Liveness handled by the B2BUA's own OPTIONS keepalive | Out of scope for now; keepalive is the liveness mechanism. Revisit if a strict-ST peer is required. |

## Verified bug status (code-traced, not just RFC-inferred)

The items below were traced through the implementation. Only **one** is a genuine
latent defect; the rest are **correct-but-untested** (test debt, not bugs).

| Case | Verdict | Evidence |
|------|---------|----------|
| ACK-4 / RTX-5 — 2xx un-ACKed by caller | **REAL BUG** | No 2xx retransmit (`sip-txn/src/layer.rs:744` only retransmits while txn `is_active`; 2xx → `Completed`) and no ACK-timeout detector. An answered/bridged call with a never-ACKing caller leaks until the 1 h `GlobalDuration` cap (or keepalive-timeout on the B-leg). RFC 3261 §13.3.1.4 (retransmit 2xx, then BYE) unimplemented. Slow leak, not unbounded. |
| REINV-4 — failed re-INVITE (provisional then final) | **NARROW LATENT GAP** | Happy path is correct (`relay-reinvite-response` overrides `route-failure`, keeps the call up — `b2bua/src/rules/defaults.rs:178-191`). But the pending snapshot is removed on *any* matched response incl. a provisional (`actions.rs:786-814`); a re-INVITE that sends `18x` then `488` loses the snapshot on the 18x, so the 488 falls through to `route-failure` → `TerminateCall`. Edge-of-an-edge (re-INVITEs rarely send provisionals). |
| CANCEL-2 — late A CANCEL after B answered | Correct, untested | Post-2xx CANCEL gets 481 at txn layer (no event); in-window, `handle-cancel` BYEs a Confirmed B-leg, never CANCELs (`defaults.rs:582-599`, `actions.rs:1250-1264`). |
| CANCEL-6 — B-leg CANCEL/2xx glare | Correct, untested | `cancel-200-crossing` rule: ConfirmDialog → ACK → BYE (`defaults.rs:112-128`). |
| REFER-SCOPE-4 — REFER retransmission | Correct, untested | Absorbed at txn layer (`layer.rs:1136-1143`, cached-202 replay); transfer code never sees the dup. |
| REFER-NOTIFY-9 — no NOTIFY after terminal | Correct, untested | Terminal NOTIFY clears the transfer slice → cursor removed → all NOTIFY-emitting transfer rules deactivated (`executor.rs:21-27`). |
| ID-1/2/3, HDR-2 — per-leg identity | Correct, untested | `build_b_leg` mints fresh Call-ID/From-tag/CSeq=1 and rewrites Contact to the B2BUA (`relay.rs:195-246`, `stack_identity.rs:45-59`). |

## Priority recommendations (what to add first)

**P0 — fix the one real defect**
1. **ACK-4 / RTX-5** — implement §13.3.1.4: retransmit the 2xx (T1→T2, give up at 64·T1), and on no-ACK send BYE on the A-leg + tear down the B-leg. Add `aleg_2xx_no_ack_retransmit_then_bye`.
2. **REINV-4 narrow gap** — decide whether the pending-request snapshot should survive a provisional and only clear on the final; add `reinvite_18x_then_488_keeps_call`.

**P1 — regression tests for correct-but-untested behaviour** (lock it in; no code change)
3. `cancel_after_2xx_byes_confirmed_bleg` (CANCEL-2) + `bleg_cancel_glare_2xx_ack_then_bye` (CANCEL-6)
4. `failed_reinvite_keeps_dialog_state` (REINV-4 happy path, the 488-without-provisional case)
5. Explicit per-leg identity asserts on `build_b_leg`: Call-ID≠A, tag independence, CSeq=1, Contact-rewrite (ID-1/2/3, HDR-2)
6. `no_notify_after_terminated` (REFER-NOTIFY-9). (REFER-SCOPE-4 needs a harness primitive to resend an identical branch/CSeq — defer until the DSL supports it.)

**P2 — robustness / forward-looking**
7. `torture_indialog_bad_msg_no_call_leak` (TORTURE-8)
8. `b2bua_self_loop_detected` + `merged_invite_482_second_copy` (LOOP-1, MERGE-1)
9. `bleg_3xx_followed_not_leaked_to_aleg` + redirect recursion bound (RDR-1/2)
10. UPDATE guards (UPDATE-ALLOWED-1, UPDATE-PENDING-1)
11. RTP anchoring corner cases if/when media is anchored (RTP-4/6/8)
12. Reason-header propagation across legs (REASON-1/2)

> Items previously listed under "P1 — unimplemented" (Replaces, auth, session
> timers) are now recorded under **Deliberate non-goals** above.
