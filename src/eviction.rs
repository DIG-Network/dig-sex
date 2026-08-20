//! [`TieredPolicy`] — the tier model wired into `dig-store-cache`'s existing eviction seam.
//!
//! `dig-store-cache` has shipped a pluggable `EvictionPolicy` trait since v0.1.0 whose only
//! implementation is `LruPolicy`, and the relevance model this crate folds in describes itself as
//! *"the pure brain the on-disk LRU cache **will later consult**"*. **The two were built to meet and
//! never did.** This module is that meeting point, which is why it IMPLEMENTS the existing trait
//! rather than designing a rival seam (SPEC §11.1).
//!
//! # What changes for an operator
//!
//! Under `LruPolicy` a full cache sacrifices the coldest entry, whatever it is — so a speculative
//! precache the node fetched on a hunch outlives a capsule a user actually read, purely by being
//! touched more recently. Under [`TieredPolicy`] eviction runs the capacity ladder: all `Tier0`
//! before any `Tier1`, and all `Tier1` before any `Tier2`, with the mirror-count objective ordering
//! each tier internally. That is an observable difference in which capsules survive a full cache, not
//! a log line (SPEC §10).
//!
//! # SPEC §7.3 — why this policy ignores `EvictionEntry::last_access`
//!
//! `dig-store-cache` bumps its recency stamp inside `Cache::get`, and `get` is the SAME call the
//! serving path makes for an **inbound peer request**. On a serving node that makes `last_access` an
//! attacker-chosen value: a peer that repeatedly requests its own content keeps it resident and lets
//! another peer's content go cold — the eviction order becomes a thing peers vote on.
//!
//! The field cannot be repaired from this side, because by the time it arrives the local read and the
//! inbound serve are indistinguishable. So this policy **never reads it**. Recency still influences
//! eviction, but only through the relevance score, whose `reads_recency_ticks` input is supplied by
//! the caller and MUST be attributed to LOCAL reads. One signal, one place, one attribution rule.
//!
//! This defect is live in `dig-store-cache` today and is harmless only because that crate has no
//! consumers; folding the cache decisions in here without addressing it is the moment it would have
//! become real.

use std::sync::Arc;

use dig_store_cache::{CapsuleIdentity, EvictionContext, EvictionEntry, EvictionPolicy};

use crate::algorithm::AlgorithmSet;
use crate::selection::{
    select_within_capacity, DisplacementMargin, SelectionCandidate, SelectionPolicy, SelectionSeed,
};

/// Eviction driven by the tier ladder and the mirror-count objective.
///
/// Holds the composed [`AlgorithmSet`] that answers what tier each capsule is in, and the node-local
/// [`SelectionSeed`] that decorrelates this node's tiebreaks from every other node's (SPEC §4.4).
pub struct TieredPolicy {
    algorithms: Arc<AlgorithmSet<CapsuleIdentity>>,
    seed: SelectionSeed,
    margin: DisplacementMargin,
}

impl TieredPolicy {
    /// Build a policy over a composed algorithm set and a node-local seed, with the displacement
    /// margin at SPEC §9's default.
    #[must_use]
    pub fn new(algorithms: Arc<AlgorithmSet<CapsuleIdentity>>, seed: SelectionSeed) -> Self {
        Self {
            algorithms,
            seed,
            margin: DisplacementMargin::default(),
        }
    }

    /// Raise the displacement margin this policy selects under.
    ///
    /// It is carried for the caller that shares one configured margin across every selection this
    /// node runs. **At this seam it cannot change an outcome** — see [`Self::candidate`] for why —
    /// so raising it here is about keeping one configured value, not about tightening eviction.
    #[must_use]
    pub fn with_margin(self, margin: DisplacementMargin) -> Self {
        Self { margin, ..self }
    }

    /// The capacity the incumbents are selected against: total capacity less the capsule being
    /// admitted. Selecting incumbents into the REMAINING space is what makes the returned rejects
    /// free exactly enough room for the incoming capsule.
    fn incumbent_capacity(ctx: &EvictionContext<'_>) -> u64 {
        ctx.capacity.saturating_sub(ctx.incoming_size)
    }

    /// Turn a cache entry into a selection candidate. `entry.last_access` is deliberately dropped —
    /// see the module docs (SPEC §7.3).
    ///
    /// **Every entry here is `resident`, and that makes the displacement margin inert at this seam**
    /// (SPEC §3.2). `EvictionContext` describes the incoming capsule by `incoming_size` alone — it
    /// carries no identity and no score — so the challenger is not a candidate and there is no
    /// incumbent-versus-challenger comparison for a margin to gate. Discounting a uniformly resident
    /// set discounts nothing.
    ///
    /// That is a property of the seam `dig-store-cache` owns, not a decision taken here, and it is
    /// why hysteresis has to be applied by the caller BEFORE it asks the cache to admit anything.
    /// Marking these entries resident is what makes the value honest if that context ever grows the
    /// challenger's facts.
    fn candidate(&self, entry: &EvictionEntry) -> SelectionCandidate<CapsuleIdentity> {
        let facts = self.algorithms.facts_or_default(&entry.id);
        SelectionCandidate {
            id: entry.id,
            tier: facts.tier,
            size_bytes: entry.size,
            score: facts.score,
            pinned: entry.pinned,
            resident: true,
        }
    }
}

impl EvictionPolicy for TieredPolicy {
    /// The capsules to evict, in eviction order (lowest tier first).
    ///
    /// Never returns a pinned entry: [`select_within_capacity`] retains every pin unconditionally, so
    /// a pin can only ever appear in the retained set. That upholds the invariant `dig-store-cache`
    /// relies on and does not re-check.
    ///
    /// Every returned capsule is **owed** an advertising retraction (SPEC §7.1) — but that coupling
    /// is the caller's discipline, not this signature's guarantee. `dig-store-cache` owns the return
    /// type, so no delta can be returned here; a caller MUST pass this set through
    /// [`crate::holdings::after_eviction`] and act on the result. `dig-node` does (dig-node#280).
    fn select_evictions(&self, ctx: &EvictionContext<'_>) -> Vec<CapsuleIdentity> {
        if ctx.bytes_to_free() == 0 {
            return Vec::new();
        }
        let candidates: Vec<_> = ctx.entries.iter().map(|e| self.candidate(e)).collect();
        let policy =
            SelectionPolicy::new(Self::incumbent_capacity(ctx), self.seed).with_margin(self.margin);
        select_within_capacity(&candidates, policy).rejected
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::algorithm::{ExchangeAlgorithm, StoreFacts};
    use crate::relevance::RelevanceValue;
    use crate::tier::CacheTier;

    fn id(byte: u8) -> CapsuleIdentity {
        CapsuleIdentity {
            store_id: [byte; 32].into(),
            root_hash: [byte; 32].into(),
        }
    }

    /// Claims a fixed tier for a fixed set of capsules.
    struct Tiered(Vec<(CapsuleIdentity, CacheTier)>);

    impl ExchangeAlgorithm<CapsuleIdentity> for Tiered {
        fn facts(&self, wanted: &CapsuleIdentity) -> Option<StoreFacts> {
            self.0
                .iter()
                .find(|(held, _)| held == wanted)
                .map(|(_, tier)| StoreFacts {
                    tier: *tier,
                    score: RelevanceValue(0.0),
                })
        }
    }

    fn policy(claims: Vec<(CapsuleIdentity, CacheTier)>) -> TieredPolicy {
        TieredPolicy::new(
            Arc::new(AlgorithmSet::new().with(Box::new(Tiered(claims)))),
            SelectionSeed::from_node_local(0x5E1E_C712_0000_0001),
        )
    }

    fn entry(byte: u8, size: u64, last_access: u64, pinned: bool) -> EvictionEntry {
        EvictionEntry {
            id: id(byte),
            size,
            last_access,
            pinned,
        }
    }

    /// The core behaviour change over `LruPolicy`: a speculative capsule is sacrificed before a
    /// demanded one even though it is the MOST recently used entry. Under `LruPolicy` this fixture
    /// evicts the demanded capsule, so the assertion distinguishes the two policies rather than
    /// restating what the cache already did.
    #[test]
    fn a_fresh_precache_capsule_is_evicted_before_a_stale_demanded_one() {
        let policy = policy(vec![
            (id(0), CacheTier::Tier0Precache),
            (id(1), CacheTier::Tier1Demand),
        ]);
        let entries = [entry(0, 100, 999, false), entry(1, 100, 1, false)];
        let ctx = EvictionContext {
            entries: &entries,
            current_bytes: 200,
            capacity: 200,
            incoming_size: 100,
        };

        assert_eq!(policy.select_evictions(&ctx), vec![id(0)]);
    }

    /// SPEC §7.3 — the load-bearing test. Two contexts differ ONLY in `last_access`, arranged so that
    /// an attacker's repeated inbound requests have made THEIR capsule the hottest and the honest
    /// one the coldest. The eviction set must be identical, because this policy never reads the
    /// field. A policy that ordered on it returns a different victim for the two contexts.
    ///
    /// Both capsules sit in the SAME tier with the SAME size: without that, the tier ladder or the
    /// size objective would decide the outcome and the test would pass whether or not `last_access`
    /// was consulted.
    #[test]
    fn inbound_driven_recency_cannot_steer_eviction() {
        let policy = policy(vec![
            (id(0), CacheTier::Tier1Demand),
            (id(1), CacheTier::Tier1Demand),
        ]);

        let honest_is_hot = [entry(0, 100, 999, false), entry(1, 100, 1, false)];
        let attacker_is_hot = [entry(0, 100, 1, false), entry(1, 100, 999, false)];

        let evict = |entries: &[EvictionEntry; 2]| {
            policy.select_evictions(&EvictionContext {
                entries,
                current_bytes: 200,
                capacity: 200,
                incoming_size: 100,
            })
        };

        assert_eq!(
            evict(&honest_is_hot),
            evict(&attacker_is_hot),
            "eviction order must not move when only the inbound-drivable recency stamp changes"
        );
    }

    /// The invariant `dig-store-cache` relies on and does not re-check: a pinned entry is never
    /// returned, even when it is the only thing that could free enough room.
    #[test]
    fn a_pinned_entry_is_never_selected_even_when_nothing_else_can_free_room() {
        let policy = policy(vec![(id(0), CacheTier::Tier0Precache)]);
        let entries = [entry(0, 200, 1, true)];
        let ctx = EvictionContext {
            entries: &entries,
            current_bytes: 200,
            capacity: 200,
            incoming_size: 100,
        };

        assert!(policy.select_evictions(&ctx).is_empty());
    }

    /// Nothing is evicted when the incoming capsule already fits — the cache must not churn.
    #[test]
    fn nothing_is_evicted_when_the_incoming_capsule_fits() {
        let policy = policy(vec![(id(0), CacheTier::Tier0Precache)]);
        let entries = [entry(0, 10, 1, false)];
        let ctx = EvictionContext {
            entries: &entries,
            current_bytes: 10,
            capacity: 1000,
            incoming_size: 10,
        };

        assert!(policy.select_evictions(&ctx).is_empty());
    }

    /// SPEC §2.2 fail-safe: a capsule no algorithm claims defaults to the PROTECTED tier, so an
    /// unclaimed capsule outlives a claimed speculative one rather than being sacrificed first.
    #[test]
    fn an_unclaimed_capsule_is_protected_against_a_claimed_speculative_one() {
        let policy = policy(vec![(id(0), CacheTier::Tier0Precache)]);
        let entries = [entry(0, 100, 1, false), entry(9, 100, 1, false)];
        let ctx = EvictionContext {
            entries: &entries,
            current_bytes: 200,
            capacity: 200,
            incoming_size: 100,
        };

        assert_eq!(policy.select_evictions(&ctx), vec![id(0)]);
    }
}
