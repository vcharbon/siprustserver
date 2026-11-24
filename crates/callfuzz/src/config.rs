//! The knobs of a fuzz run, as the caller states them.

/// What a fuzz run draws and how far it may push a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FuzzConfig {
    /// The run's seed: with the call's token, each call's ([`crate::call_seed`]).
    pub run_seed: u64,
    /// Replay one call: every call draws this seed instead of its own.
    pub call_seed: Option<u64>,
    /// Percent of calls carrying hostile headers and odd INFO bodies.
    pub payload_pct: u64,
    /// Percent of odd INFOs requiring an extension no party implements: of
    /// their UAS (`Require`), or, on a path through a proxy, of the proxy
    /// (`Proxy-Require`), which a UAS ignores.
    pub unknown_require_pct: u64,
    /// The calls cross a proxy that Record-Routes.
    pub proxied: bool,
    /// Percent of calls whose goal dwells are jittered.
    pub jitter_pct: u64,
    /// Percent of calls whose offer mode and body framings are drawn.
    pub offer_pct: u64,
    /// Percent of calls whose caller holds an ACK to a 2xx, and, drawn apart,
    /// whose parties hold or refuse the renegotiations they take.
    pub hold_pct: u64,
    /// A party drawn to answer renegotiations late may refuse them too.
    pub refuse: bool,
    /// The longest drawn PRACK hold. The default, 3000 ms, is past the
    /// provisional's third re-send at T1 = 500 ms and inside the 64·T1 its
    /// sender re-sends it for (RFC 3262 §3); a call whose answer timing its
    /// expected ending derives from states a shorter one.
    pub prack_hold_max_ms: u64,
    /// Bytes the fuzzer may add to one INVITE's headers, so the message stays
    /// inside the receiver's size bound with room for what a proxy adds.
    pub header_budget: usize,
    /// Bodies an RFC refuses may be drawn (a multipart boundary outside
    /// RFC 2046 §5.1.1's `bcharsnospace`).
    pub illegal: bool,
    /// Every held callee rings a drawn provisional train.
    pub train: bool,
    /// Provisionals in one callee's train, at most.
    pub train_max: usize,
    /// Each call's draw is written to stderr as one `FUZZ-DRAW` line.
    pub log_draws: bool,
    /// The roles that send no odd INFO: a call that refuses a party's INFO.
    pub quiet: &'static [&'static str],
    /// The fields of an initial INVITE a payload call may draw into
    /// ([`crate::identity`]).
    pub targets: Targets,
    /// The name-addr headers ([`AddrForm`](sip_message::header::AddrForm)
    /// `name-addr / addr-spec`) a payload call may add, each with a drawn
    /// display name and user.
    pub addr_headers: &'static [&'static str],
    /// The free-text headers (an extension header, `Subject`, `Organization`)
    /// a payload call may add, each with a drawn value.
    pub text_headers: &'static [&'static str],
    /// The drawn names and values may hold every character their grammar
    /// slot allows ([`crate::chars`]).
    pub all_chars: bool,
}

/// The fields of an initial INVITE the fuzzer may draw into, beyond its
/// hostile extension headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Targets {
    /// The From display name.
    pub from: bool,
    /// The To display name.
    pub to: bool,
    /// A preloaded `Route` naming the request's first hop (RFC 3261 §8.1.2).
    pub route: bool,
    /// The wire spelling of the headers the stack writes: compact forms and
    /// any case (RFC 3261 §7.3.1, §7.3.3).
    pub names: bool,
}

impl Targets {
    /// Every target.
    pub const ALL: Targets = Targets { from: true, to: true, route: true, names: true };
    /// No target.
    pub const NONE: Targets = Targets { from: false, to: false, route: false, names: false };

    /// The targets a comma list names among `from`, `to`, `route` and
    /// `names`; `none` alone names none. Any other word is refused.
    pub fn parse(list: &str) -> Result<Self, String> {
        let mut targets = Targets::NONE;
        for word in list.split(',').map(str::trim).filter(|w| !w.is_empty()) {
            match word {
                "from" => targets.from = true,
                "to" => targets.to = true,
                "route" => targets.route = true,
                "names" => targets.names = true,
                "none" => {}
                other => {
                    return Err(format!("{other:?} is none of from, to, route, names and none"))
                }
            }
        }
        Ok(targets)
    }
}

impl Default for FuzzConfig {
    /// Run seed 1, payloads on half the calls, jitter on 70 %, offer modes and
    /// framings on half, a held ACK on a fifth, a 6000-byte
    /// header budget, legal messages only, no trains unless asked (30 at most),
    /// every target, `P-Asserted-Identity` as the name-addr header, no
    /// free-text header, no all-chars values,
    /// every party free to send an odd INFO.
    fn default() -> Self {
        FuzzConfig {
            run_seed: 1,
            call_seed: None,
            payload_pct: 50,
            jitter_pct: 70,
            unknown_require_pct: 25,
            proxied: false,
            offer_pct: 50,
            hold_pct: 20,
            refuse: true,
            prack_hold_max_ms: 3000,
            header_budget: 6000,
            illegal: false,
            train: false,
            train_max: 30,
            log_draws: false,
            quiet: &[],
            targets: Targets::ALL,
            addr_headers: &["P-Asserted-Identity"],
            text_headers: &[],
            all_chars: false,
        }
    }
}

impl FuzzConfig {
    /// The seed the call of `token` draws from.
    pub fn seed_of(&self, token: &str) -> u64 {
        crate::call_seed(self.run_seed, self.call_seed, token)
    }
}
