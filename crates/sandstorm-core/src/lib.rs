//! sandstorm v0.6: one binary, two modes.
//!
//! `sandstorm serve --master` runs the control plane: sqlite desired state, the
//! public API, the CA, and the idle-demotion loop. `sandstorm serve --node` runs
//! the sandbox agent: it dials the master and executes tier changes against
//! runsc. Passing both runs a single-machine deployment in one process,
//! which is the common case and the one worth making easy.

pub mod audit;
pub mod auth;
pub mod identity;
pub mod master;
pub mod node;
pub mod proto;
pub mod proxy;
pub mod state;
pub mod tag;
pub mod tier;
pub mod tls;
