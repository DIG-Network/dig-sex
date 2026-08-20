//! # dig-sex — Store EXchange
//!
//! The **policy layer** for exchanging DIG stores between peers. It answers *which* store to
//! exchange, *with whom*, *in what order*, and *why* — and it does so in one place, so the answer
//! stops being implicit in whichever crate happened to be holding the request.
//!
//! ## What this crate is, and what it deliberately is not
//!
//! It is **not** a transport, a discovery mechanism, or a fetcher. Those exist and stay where they
//! are: `dig-dht` finds providers, `dig-pex` exchanges peer records, `dig-download` moves bytes,
//! `dig-store-cache` holds what arrived. This crate **composes** them behind one decision surface.
//!
//! Store exchange already happens today, spread across those crates and dig-node's own
//! subscription and sync paths, with relevance decided differently in each. Centralising it is the
//! point: one place to reason about, one place to change, and — critically — one place where a
//! *different* exchange algorithm can be swapped in without touching the layers beneath.
//!
//! ## Pluggable algorithms are the primary requirement, not a later refinement
//!
//! The exchange strategy is an interface, and at least one future implementation is **incentivised**
//! exchange — peers trading on terms rather than on relevance alone. A design that hard-codes any
//! single policy has failed its main purpose, so the seam comes first and the first algorithm is
//! merely the first implementation of it.
//!
//! ## Trust
//!
//! Every peer is untrusted (NC-12). An exchange decision reads peer-supplied claims — what a peer
//! says it holds, what it says it wants — and **a claim is not evidence**. Content is accepted
//! because it verifies against its on-chain-anchored root, never because a peer offered it, and no
//! ranking may promote a candidate on the strength of a declaration.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// Placeholder while the exchange layer is designed against the existing crates it composes.
///
/// Removed by the first real implementation; present so the crate compiles and publishes its
/// scaffolding while the measurement of the current exchange surfaces completes.
pub const DESIGN_PENDING: bool = true;
