//! Two active proxy instances behind one advertised address (ADR-0009 X7):
//! an ECMP hop hashes each flow on its own, so the two sides of one
//! transaction can cross different instances. Each cell assigns every
//! party's flow to an instance and sends a transaction's request, or its
//! ACK, to the instance whose branch memo holds nothing of it.

mod caller_side;
mod dialog;
mod health_views;
mod memo_life;
mod parties;
mod residual;
mod rig;
mod worker_side;
