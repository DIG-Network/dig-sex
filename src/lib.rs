//! # dig-sex — Store EXchange
//!
//! The **policy layer** for exchanging DIG stores between peers, and the home of the
//! cache/relevance/tier system that decides what a node holds. It answers *which* store to acquire,
//! *with whom*, *what to keep*, *what to sacrifice first*, and *why*.
//!
//! ## The objective function
//!
//! **Profit first; subject to that, maximise the NUMBER of mirrors within the disk allocation.** The
//! two are lexicographic, never weighted: no quantity of mirrors justifies sacrificing paid
//! retention, and this crate provides no way to express an exchange rate between them.
//!
//! Everything here is machinery for that one sentence:
//!
//! | module | its part of the objective |
//! |---|---|
//! | [`tier`] | the profit ladder — a capacity ladder in which higher tiers claim disk first |
//! | [`relevance`] | desirability WITHIN a tier, scored purely and deterministically |
//! | [`selection`] | the mirror-count objective — a per-tier knapsack over residual capacity |
//! | [`algorithm`] | the pluggable seam: how a candidate earns a tier and how it scores |
//! | [`eviction`] | the above, wired into `dig-store-cache`'s existing eviction seam |
//! | [`acquisition`] | a remotely-satisfied read creates relevance and warms the whole capsule |
//! | [`holdings`] | every eviction is also an advertising retraction |
//!
//! ## What this crate is deliberately not
//!
//! Not a transport, not a discovery mechanism, not a fetcher, and not a verifier. `dig-dht` finds
//! providers, `dig-pex` exchanges peer records, `dig-download` moves bytes, `dig-store-cache`
//! performs the on-disk admission and eviction **mechanics**. This crate owns the **decisions** those
//! mechanisms carry out. *"Stage to a temp file, fsync, rename"* is mechanism; *"which store, from
//! whom, and what do I drop to make room"* is policy.
//!
//! It also does not re-implement anything a composed crate owns. Where such a crate already exposes a
//! decision seam, this crate implements it — [`eviction::TieredPolicy`] is an implementation of
//! `dig-store-cache`'s `EvictionPolicy`, not a rival to it.
//!
//! ## The paid tier exists; its algorithm does not
//!
//! [`CacheTier::Tier2Bribed`](tier::CacheTier::Tier2Bribed) is part of the model **now** — it is the
//! reason eviction has a ladder at all. The algorithm that decides who pays, how much, and what
//! proves it is **deferred**, and nothing in this crate produces that variant. See [`algorithm`] for
//! how such an algorithm is added later without any signature here changing.
//!
//! Until it lands, the primary objective means exactly one thing and no more: **never sacrifice paid
//! content to hold unpaid content.**
//!
//! ## Trust
//!
//! Every peer is untrusted (NC-12). An exchange decision reads peer-supplied claims — what a peer says
//! it holds, wants, or will pay — and **a claim is not evidence**. No ranking input a peer supplies
//! may promote a candidate, an absent value never outranks a present one, and no recency signal
//! drivable by inbound requests is allowed to order eviction (see [`eviction`]).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod acquisition;
pub mod algorithm;
pub mod eviction;
pub mod holdings;
pub mod relevance;
pub mod selection;
pub mod tier;

pub use acquisition::{AcquisitionDecision, BackfillPolicy};
pub use algorithm::{AlgorithmSet, ExchangeAlgorithm, StoreFacts};
pub use eviction::TieredPolicy;
pub use holdings::HoldingsDelta;
pub use relevance::{
    relevance, NodeContext, RelevanceInputs, RelevanceValue, RelevanceWeights,
    INBOUND_DEMAND_MIN_PROXIMITY,
};
pub use selection::{select_within_capacity, Selection, SelectionCandidate, SelectionSeed};
pub use tier::{effective_tier, evict_key, CacheEntry, CacheTier, DEFAULT_TIER};

/// The capsule identity every DIG surface speaks, re-exported so a consumer of this crate never
/// declares its own and never pulls a second major of `dig-store` into the graph (SPEC §9).
pub use dig_store_cache::CapsuleIdentity;
