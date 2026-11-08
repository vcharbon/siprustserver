//! The environment faults a bridge met: what a test fails on before any
//! verdict about the system under test.

use std::fmt;
use std::sync::Mutex;
use std::time::Duration;

use crate::poison::locked;

/// One environment fault.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnvFault {
    /// The real work had no result `cap` of real time after it fell due.
    Stall { label: String, cap: Duration },
    /// A stalled work's result arrived anyway, `after` its start.
    Late { label: String, after: Duration },
    /// The external process failed the exchange (refused, reset, closed).
    Failed { label: String, detail: String },
}

impl fmt::Display for EnvFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stall { label, cap } => {
                write!(f, "external stall: {label}: no result {cap:?} after it fell due")
            }
            Self::Late { label, after } => {
                write!(f, "late result: {label}: arrived {after:?} after its start")
            }
            Self::Failed { label, detail } => write!(f, "external failure: {label}: {detail}"),
        }
    }
}

/// The faults of one bridge, in the order met.
#[derive(Debug, Default)]
pub struct FaultLog(Mutex<Vec<EnvFault>>);

impl FaultLog {
    /// Record `fault`.
    pub fn record(&self, fault: EnvFault) {
        locked(&self.0).push(fault);
    }

    /// Every fault so far.
    pub fn faults(&self) -> Vec<EnvFault> {
        locked(&self.0).clone()
    }
}
