//! The delivery engine's pure decisions: what a submission's answer, a lost lease, a Start's final
//! checks and preflight do to a message, its recipients, its connection and its quota scope
//! ([`delivery`]); and how much a connection may claim and how long a spent budget waits
//! ([`budget`]).
//!
//! Every function takes facts (rows already read, the transport's answer, `now`, a random draw for
//! jitter) and returns a decision; the operations in `delivery/` read the facts, call these, and
//! write the result in the transaction that owns it. The tables are tested over every variant of
//! their enums, so a new outcome, phase, scope, cause or category fails the tests until its row is
//! decided.

pub mod budget;
pub mod delivery;
