//! The pluggable seam (SPEC §2.4, §11A.4) — how a candidate earns a tier, and how it scores within one.
//!
//! The tier ladder ([`crate::tier`]) and its precedence are the **fixed frame**. What plugs in is
//! everything above it: an [`ExchangeAlgorithm`] answers, for one store, *which tier does it hold*
//! and *how desirable is it within that tier*. Nothing else about a store is a policy decision.
//!
//! # Several algorithms run at once, and that is the design
//!
//! This is a set of tiered acquisition sources competing for one capacity budget, not one algorithm
//! with one policy. A store can be speculatively precached AND locally read AND paid for, all at the
//! same time, by three different algorithms that know nothing about each other. [`AlgorithmSet`]
//! composes them the only way the model permits: the effective tier is the **maximum** across every
//! source that claims the store (SPEC §2.2), so a claim can only ever promote, never demote.
//!
//! # How `Tier2Bribed` gets populated later, without any signature here changing
//!
//! The paid tier is part of the model now; the algorithm that decides who pays and what proves it is
//! **deferred** (SPEC §2.4) and is deliberately not implemented in this crate. When it lands it is
//! *one more* [`ExchangeAlgorithm`] added to an [`AlgorithmSet`], returning
//! [`CacheTier::Tier2Bribed`](crate::tier::CacheTier::Tier2Bribed) for the stores it has settled
//! payment for. It needs no new trait, no new method, and no change to eviction or selection:
//!
//! - **promotion** travels the existing MAX composition — settling payment raises the tier;
//! - **demotion on non-payment** travels the SAME channel every other source uses — the algorithm
//!   stops claiming the store, its claim drops out of the max, and the store falls back to whatever
//!   the remaining sources say. Non-payment therefore never has to live in private state, which is
//!   the interface defect SPEC §2.4 exists to prevent;
//! - **price, payer and settlement outcome** are that algorithm's own inputs, not this seam's. This
//!   seam is deliberately narrow: it carries the DECISION, never the evidence behind it.
//!
//! What this crate does NOT do is invent that algorithm's terms. No implementation here returns
//! `Tier2Bribed`.

use crate::relevance::RelevanceValue;
use crate::tier::{effective_tier, CacheTier, DEFAULT_TIER};

/// What an algorithm asserts about one store.
///
/// There is deliberately no recency field. Recency is already a term of the relevance model
/// (`RelevanceInputs::reads_recency_ticks`) and belongs in the score, so there is exactly one place a
/// recency signal enters policy — and exactly one place to keep it locally attributed (SPEC §7.3).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StoreFacts {
    /// The tier this algorithm claims for the store.
    pub tier: CacheTier,
    /// How desirable the store is WITHIN that tier. Never compared across tiers.
    pub score: RelevanceValue,
}

/// One pluggable exchange algorithm, keyed by whatever identifier the caller's cache uses.
///
/// An implementation returns `None` for a store it holds no opinion about. `None` is not a demotion
/// and MUST NOT be read as one: it removes the algorithm's claim from the composition, leaving the
/// other sources to answer (SPEC §2.2 — a promotion survives one reason lapsing).
pub trait ExchangeAlgorithm<Id>: Send + Sync {
    /// This algorithm's claim about `id`, or `None` if it holds no opinion.
    fn facts(&self, id: &Id) -> Option<StoreFacts>;
}

/// Several algorithms competing for one capacity budget (SPEC §11A.4).
///
/// The composition rule is fixed by the model and is not itself pluggable: **tier is the maximum**
/// across claiming sources, and the score is the one belonging to the winning tier's strongest claim.
/// Taking the score from the winning tier rather than the global maximum matters — a score is only
/// meaningful within its own tier, so a high score attached to a `Tier0` claim must not follow a store
/// that a different source promoted to `Tier1`.
pub struct AlgorithmSet<Id> {
    sources: Vec<Box<dyn ExchangeAlgorithm<Id>>>,
}

impl<Id> AlgorithmSet<Id> {
    /// An empty set. Every store then resolves to [`DEFAULT_TIER`] with a zero score — the fail-safe
    /// answer, never an empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self {
            sources: Vec::new(),
        }
    }

    /// Add an algorithm. Order does not affect the outcome: composition is a maximum, not a
    /// last-writer-wins, so a caller cannot change policy by reordering registration.
    #[must_use]
    pub fn with(mut self, source: Box<dyn ExchangeAlgorithm<Id>>) -> Self {
        self.sources.push(source);
        self
    }

    /// The effective facts for `id`: the maximum tier across claiming sources, with the best score
    /// claimed AT that tier.
    ///
    /// Returns `None` when no source claims the store. Callers pair that with [`DEFAULT_TIER`] via
    /// [`AlgorithmSet::facts_or_default`]; it is not defaulted here so "nobody claimed this" stays
    /// distinguishable from "somebody claimed the default".
    #[must_use]
    pub fn facts(&self, id: &Id) -> Option<StoreFacts> {
        let claims: Vec<StoreFacts> = self.sources.iter().filter_map(|s| s.facts(id)).collect();
        let winning_tier = effective_tier(claims.iter().map(|c| c.tier))?;
        let score = claims
            .iter()
            .filter(|c| c.tier == winning_tier)
            .map(|c| c.score.get())
            .fold(f64::NEG_INFINITY, f64::max);
        Some(StoreFacts {
            tier: winning_tier,
            score: RelevanceValue(score),
        })
    }

    /// [`AlgorithmSet::facts`] with the fail-safe default applied: an unclaimed store is treated as
    /// [`DEFAULT_TIER`] (protected) with a zero score, so it is neither sacrificed first nor allowed
    /// to outrank a store an algorithm actually vouched for.
    ///
    /// The zero score is the SPEC §8.2 rule made concrete: an absent value must not outrank a present
    /// one, so absence takes the bottom of its tier rather than the top.
    #[must_use]
    pub fn facts_or_default(&self, id: &Id) -> StoreFacts {
        self.facts(id).unwrap_or(StoreFacts {
            tier: DEFAULT_TIER,
            score: RelevanceValue(0.0),
        })
    }
}

impl<Id> Default for AlgorithmSet<Id> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stub algorithm that claims exactly the ids it was given.
    struct Claims(Vec<(u8, CacheTier, f64)>);

    impl ExchangeAlgorithm<u8> for Claims {
        fn facts(&self, id: &u8) -> Option<StoreFacts> {
            self.0
                .iter()
                .find(|(claimed, _, _)| claimed == id)
                .map(|(_, tier, score)| StoreFacts {
                    tier: *tier,
                    score: RelevanceValue(*score),
                })
        }
    }

    fn set(sources: Vec<Claims>) -> AlgorithmSet<u8> {
        sources
            .into_iter()
            .fold(AlgorithmSet::new(), |acc, source| {
                acc.with(Box::new(source))
            })
    }

    /// SPEC §2.2. Two independent algorithms claim the same store at different tiers; the promoted
    /// tier wins, and it wins in BOTH registration orders — a last-writer-wins composition passes one
    /// order and fails the other, which is why both are asserted.
    #[test]
    fn composition_takes_the_maximum_tier_in_either_registration_order() {
        let precache = || Claims(vec![(1, CacheTier::Tier0Precache, 5.0)]);
        let demand = || Claims(vec![(1, CacheTier::Tier1Demand, 1.0)]);

        let forward = set(vec![precache(), demand()]).facts(&1).unwrap();
        let reverse = set(vec![demand(), precache()]).facts(&1).unwrap();

        assert_eq!(forward.tier, CacheTier::Tier1Demand);
        assert_eq!(reverse.tier, CacheTier::Tier1Demand);
    }

    /// The score must come from the WINNING tier's claim, not the global best. Here the losing
    /// `Tier0` claim carries the higher score, so an implementation that took the global maximum
    /// would carry a tier-0 desirability into a tier-1 ranking — comparing scores across tiers, which
    /// the model forbids.
    #[test]
    fn the_score_comes_from_the_winning_tier_not_the_global_best() {
        let composed = set(vec![
            Claims(vec![(1, CacheTier::Tier0Precache, 99.0)]),
            Claims(vec![(1, CacheTier::Tier1Demand, 2.0)]),
        ]);
        let facts = composed.facts(&1).unwrap();

        assert_eq!(facts.tier, CacheTier::Tier1Demand);
        assert_eq!(facts.score.get(), 2.0);
    }

    /// SPEC §2.2: a source going silent is not a demotion. The promoting source alone still yields
    /// the promoted tier once the speculative source stops claiming the store.
    #[test]
    fn a_source_going_silent_does_not_demote() {
        let after_precache_lapses = set(vec![
            Claims(vec![]),
            Claims(vec![(1, CacheTier::Tier1Demand, 1.0)]),
        ]);
        assert_eq!(
            after_precache_lapses.facts(&1).unwrap().tier,
            CacheTier::Tier1Demand
        );
    }

    /// SPEC §8.2: an absent value must not outrank a present one. An unclaimed store defaults to the
    /// PROTECTED tier (so it is not wrongly sacrificed) but to the BOTTOM score of that tier (so it
    /// cannot outrank a store an algorithm vouched for).
    #[test]
    fn an_unclaimed_store_is_protected_but_never_outranks_a_claimed_one() {
        let composed = set(vec![Claims(vec![(1, CacheTier::Tier1Demand, 3.0)])]);

        assert_eq!(composed.facts(&2), None);
        let defaulted = composed.facts_or_default(&2);
        assert_eq!(defaulted.tier, DEFAULT_TIER);
        assert!(defaulted.score.get() < composed.facts_or_default(&1).score.get());
    }

    /// Nothing in this crate produces the paid tier — its algorithm is deferred (SPEC §2.4). The seam
    /// must nevertheless carry it end to end, so a later algorithm is a pure addition.
    #[test]
    fn the_seam_carries_the_paid_tier_although_no_implementation_here_produces_it() {
        let future_paid_algorithm = set(vec![
            Claims(vec![(1, CacheTier::Tier1Demand, 1.0)]),
            Claims(vec![(1, CacheTier::Tier2Bribed, 0.0)]),
        ]);
        assert_eq!(
            future_paid_algorithm.facts(&1).unwrap().tier,
            CacheTier::Tier2Bribed,
            "a paid claim promotes, even carrying the lowest possible score"
        );
    }
}
