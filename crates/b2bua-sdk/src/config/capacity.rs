//! The worker's memory bounds on new calls (ADR-0037): the admission
//! ceilings on each measured quantity.

/// A pair of admission ceilings on one measured quantity. At or above
/// `normal` a new non-emergency call is refused; at or above `emergency`
/// every new call is refused. `None` leaves that class unbounded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ceilings {
    pub normal: Option<u64>,
    pub emergency: Option<u64>,
}

impl Ceilings {
    /// The ceiling a call of this class is refused at: a non-emergency call
    /// meets the lower of the two, an emergency call only `emergency`.
    pub fn for_class(&self, is_emergency: bool) -> Option<u64> {
        if is_emergency {
            self.emergency
        } else {
            match (self.normal, self.emergency) {
                (Some(n), Some(e)) => Some(n.min(e)),
                (n, e) => n.or(e),
            }
        }
    }

    /// `normal` and `emergency` name the runner variables stating the pair.
    fn validate(&self, normal: &str, emergency: &str) -> Result<(), String> {
        if self.normal == Some(0) || self.emergency == Some(0) {
            return Err(format!(
                "{normal} / {emergency}: a ceiling of 0 refuses every call; unset it"
            ));
        }
        if let (Some(n), Some(e)) = (self.normal, self.emergency) {
            if e < n {
                return Err(format!(
                    "{emergency}={e} is below {normal}={n}; emergency calls keep priority, so \
                     their ceiling must be the higher one"
                ));
            }
        }
        Ok(())
    }
}

/// The worker's memory bounds on new calls (ADR-0037). Live calls and
/// transactions are counted exactly; RSS is the process resident set,
/// sampled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CapacityConfig {
    /// Live calls this worker serves.
    pub calls: Ceilings,
    /// Live SIP transactions, every role and state.
    pub transactions: Ceilings,
    /// Process resident set size, in bytes.
    pub rss_bytes: Ceilings,
}

impl CapacityConfig {
    /// Refuses a zero ceiling and an emergency ceiling below its normal one.
    pub fn validate(&self) -> Result<(), String> {
        self.calls.validate("B2BUA_MAX_CALLS", "B2BUA_MAX_CALLS_EMERGENCY")?;
        self.transactions.validate("B2BUA_MAX_TXNS", "B2BUA_MAX_TXNS_EMERGENCY")?;
        self.rss_bytes.validate("B2BUA_MAX_RSS", "B2BUA_MAX_RSS_EMERGENCY")
    }
}
