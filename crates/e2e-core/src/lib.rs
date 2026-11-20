//! The **cell result record** (ADR-0018): the `result.json` a crate's in-test
//! reporter writes for one cell of a campaign, which the campaign runner reads
//! back and `ts/contracts/src/e2e.ts` mirrors.

pub mod result;

pub use result::{CellId, RunResult, Timings};
