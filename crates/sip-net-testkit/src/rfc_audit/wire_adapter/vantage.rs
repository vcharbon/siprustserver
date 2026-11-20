//! Where a row reports its rule's findings: each bind's view judged, and the
//! findings kept at the lane the row's vantage names.

use super::*;

/// Run `rule` over `views` and keep the violated, non-relayed findings the
/// `vantage` predicate accepts for the bind the view belongs to, each charged
/// to the rule's `emitter` with its `taker` as the hop's other end, whatever
/// vantage reports it. Every non-relayed occasion at that vantage, violated or
/// not, is noted for the run's rule population.
fn at_vantage(
    pass: &AuditPass<'_>,
    views: &[(&view::BindView, &Observation)],
    rule: &dyn Obligation,
    vantage: impl Fn(&rfc_rules::Finding, &LaneKey, &str, &[Msg]) -> bool,
    detail: impl Fn(&rfc_rules::Finding, &str) -> String,
) -> Vec<PositionedFinding> {
    let positions = pass.positions();
    let mut out = Vec::new();
    for &(view, obs) in views {
        for f in rule.eval(&WireView { msgs: &view.msgs, obs }) {
            let (seq, cid) = &view.meta[f.anchor];
            if f.relayed || !vantage(&f, &view.bind, cid, &view.msgs) {
                continue;
            }
            crate::rfc_audit::occasions::note(&view.bind, &f);
            if !f.violated() {
                continue;
            }
            out.push((
                view.bind.clone(),
                format!("{}: {}", f.emitter, detail(&f, cid)),
                positions.get(seq).copied(),
                Some(f.emitter.clone()),
                Some(f.taker.clone()),
            ));
        }
    }
    out
}

impl Live {
    /// The row's findings over `pass`, with the binds of `scoped_out` held as
    /// recorded parties: a consumer took their traffic out of the events, so
    /// what they sent is never judged at the lane that took it. The rules
    /// judge the audit-visible events; the datagrams the loss model dropped,
    /// on arrival or on send, only tell the `…UnlessLost` vantages where a
    /// lane's traffic with a party is incomplete.
    pub(super) fn found(
        &self,
        pass: &AuditPass<'_>,
        scoped_out: &HashSet<LaneKey>,
    ) -> Vec<PositionedFinding> {
        let body = self.id.rule().body;
        let admits = self.admits;
        let views = if self.per_party {
            pass.party_views()
        } else {
            pass.views().iter().map(|(v, o)| (v, o)).collect()
        };
        let found = match self.vantage {
            Vantage::Emitter => {
                let recorded = pass.recorded().scoped(scoped_out);
                let falls_back = !self.proxy_only();
                at_vantage(
                    pass,
                    &views,
                    body,
                    |f, b, _, _| {
                        let own = !falls_back || recorded.holds(&f.emitter);
                        let at = if own { &f.emitter } else { &f.taker };
                        at == b && admits(f)
                    },
                    self.detail,
                )
            }
            Vantage::EmitterUnlessLost(explained) => {
                let recorded = pass.recorded().scoped(scoped_out);
                let dropped = pass.dropped();
                at_vantage(
                    pass,
                    &views,
                    body,
                    |f, b, cid, msgs| {
                        if recorded.holds(&f.emitter) {
                            return f.emitter == *b && admits(f);
                        }
                        f.taker == *b && admits(f) && !explained(dropped, b, f, cid, msgs)
                    },
                    self.detail,
                )
            }
            Vantage::EmitterOnly => at_vantage(
                pass,
                &views,
                body,
                |f, b, _, _| f.emitter == *b && admits(f),
                self.detail,
            ),
            Vantage::Taker => {
                at_vantage(pass, &views, body, |f, b, _, _| f.taker == *b && admits(f), self.detail)
            }
            Vantage::TakerUnlessLost(explained) => {
                let dropped = pass.dropped();
                at_vantage(
                    pass,
                    &views,
                    body,
                    |f, b, cid, msgs| {
                        f.taker == *b && admits(f) && !explained(dropped, b, f, cid, msgs)
                    },
                    self.detail,
                )
            }
            Vantage::EitherEnd => at_vantage(
                pass,
                &views,
                body,
                |f, b, _, _| (f.emitter == *b || f.taker == *b) && admits(f),
                self.detail,
            ),
            Vantage::TakerElseEmitter => {
                let recorded = pass.recorded();
                at_vantage(
                    pass,
                    &views,
                    body,
                    |f, b, _, _| {
                        let at = if recorded.holds(&f.taker) { &f.taker } else { &f.emitter };
                        at == b && admits(f)
                    },
                    self.detail,
                )
            }
        };
        match self.skip.lanes(pass) {
            None => found,
            Some(skipped) => {
                found.into_iter().filter(|(lane, ..)| !skipped.contains(lane)).collect()
            }
        }
    }
}
