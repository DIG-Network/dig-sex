//! The tier ladder — the fixed frame of the exchange model (SPEC §2).
//!
//! A cacheable store holds a [`CacheTier`], and the tier ALONE decides cross-tier eviction
//! precedence. This is a **capacity ladder**, not one ranked list: higher tiers claim disk first and
//! lower tiers occupy only the space the higher ones did not take. A [`CacheTier::Tier0Precache`]
//! entry is not "less relevant" than a [`CacheTier::Tier1Demand`] one — it is *sacrificeable first
//! regardless of relevance*, so speculative precache can never push out content a user or a paying
//! backer asked for.
//!
//! Relevance scores (see [`crate::relevance`]) order candidates **within** a tier and MUST NOT move a
//! store between tiers (SPEC §2.1).
//!
//! Everything here is pure: no clock, no network, no I/O. Time enters only as caller-supplied tick
//! counters, so any eviction decision can be replayed and audited offline (SPEC §1.3).

/// Which acquisition tier a cached entry belongs to. The tier — NOT the relevance score — decides
/// cross-tier eviction precedence: a lower tier is always sacrificed before a higher one.
///
/// The variants are ordered by [`CacheTier::rank`] and that ordering is the whole model. See
/// [`evict_key`] for how it becomes an eviction order and [`effective_tier`] for how a store that
/// earned a tier by several routes at once resolves to one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CacheTier {
    /// Speculatively fetched (DHT-neighbourhood precache). Sacrificed FIRST.
    Tier0Precache,
    /// Fetched to satisfy a real read — local, or an inbound peer request. Sacrificed only after all
    /// `Tier0`.
    Tier1Demand,
    /// Retained because a backer paid to keep it resident. Sacrificed LAST.
    ///
    /// The tier is part of the model NOW; the algorithm that decides who pays, how much, and what
    /// proves it is deliberately DEFERRED and is not implemented in this crate (SPEC §2.4). Nothing
    /// in this crate produces this variant today — it is populated by an
    /// [`ExchangeAlgorithm`](crate::algorithm::ExchangeAlgorithm) implementation added later,
    /// without any signature here changing.
    Tier2Bribed,
}

impl CacheTier {
    /// Cross-tier eviction rank: **ascending** = evicted first. `Tier0Precache` → 0 (goes first),
    /// `Tier2Bribed` → 2 (goes last). Sorting entries by `(rank, recency)` ascending therefore yields
    /// exactly tier0-oldest → … → tier2-newest, which is the eviction order.
    #[must_use]
    pub const fn rank(self) -> u8 {
        match self {
            CacheTier::Tier0Precache => 0,
            CacheTier::Tier1Demand => 1,
            CacheTier::Tier2Bribed => 2,
        }
    }
}

/// The tier a store is protected AT when no source names it (SPEC §2.2, fail-safe).
///
/// A store the node holds but which no tier source claims is treated as demanded, never as
/// speculative. The asymmetry is deliberate: mis-protecting a speculative capsule wastes disk, while
/// mis-sacrificing a demanded one destroys content a user asked for. It also makes every failure of a
/// tier source (a lost in-memory ledger, an unreadable persisted tag) fail SAFE.
pub const DEFAULT_TIER: CacheTier = CacheTier::Tier1Demand;

/// Resolve the effective tier of a store from every source that holds an opinion: the **maximum**
/// (SPEC §2.2).
///
/// A store may earn a tier by more than one route at once — acquired speculatively, then read, then
/// paid for. Taking the maximum is what makes those routes compose: reading a speculatively-acquired
/// store PROMOTES it, and the promotion survives the speculative reason lapsing, because a source
/// that stops naming the store simply drops out of the input and the higher claim still stands.
///
/// Returns `None` when no source holds an opinion; callers pair that with [`DEFAULT_TIER`]. It is
/// deliberately not defaulted here so a caller cannot silently confuse "nobody claimed this" with
/// "somebody claimed the default".
#[must_use]
pub fn effective_tier(tiers: impl IntoIterator<Item = CacheTier>) -> Option<CacheTier> {
    tiers.into_iter().max_by_key(|tier| tier.rank())
}

/// A cached entry as far as eviction is concerned: its tier and a recency stamp.
///
/// The relevance SCORE is deliberately absent — cross-tier precedence is by tier alone, and score
/// ranks only within a tier during candidate selection, never during eviction (SPEC §2.1).
///
/// `last_access_ticks` MUST be a LOCALLY-attributed recency signal. A stamp bumped by the same call
/// that serves an inbound peer request makes eviction order an attacker-chosen value (SPEC §7.3); see
/// [`crate::eviction`] for how this crate keeps that signal out of the ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheEntry {
    /// The effective acquisition tier.
    pub tier: CacheTier,
    /// Tick of last LOCAL access; smaller is older (LRU within a tier).
    pub last_access_ticks: u64,
}

/// The reference eviction sort key. Sorting entries by this key **ascending** yields all `Tier0`
/// (oldest first), then all `Tier1` (oldest first), then all `Tier2` (oldest first) — precedence
/// tier2 > tier1 > tier0, with LRU inside each tier.
///
/// **This is not the order this crate evicts in.** [`TieredPolicy`](crate::eviction::TieredPolicy)
/// reaches eviction through [`select_within_capacity`](crate::selection::select_within_capacity),
/// which walks the tiers in a fixed descending order and orders within a tier by size and score, not
/// by `last_access_ticks` — a recency signal is attacker-drivable on a serving node (SPEC §7.3). Use
/// this key for a cache that does carry a trustworthy LOCAL-read recency; do not read it as a
/// description of the policy above.
#[must_use]
pub fn evict_key(entry: &CacheEntry) -> (u8, u64) {
    (entry.tier.rank(), entry.last_access_ticks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_ranks_are_ordered() {
        assert!(CacheTier::Tier0Precache.rank() < CacheTier::Tier1Demand.rank());
        assert!(CacheTier::Tier1Demand.rank() < CacheTier::Tier2Bribed.rank());
    }

    #[test]
    fn eviction_order_is_tier0_then_tier1_then_tier2_lru_within() {
        // Deliberately shuffled input; sorted-by-evict_key IS the eviction order.
        let mut entries = [
            CacheEntry {
                tier: CacheTier::Tier2Bribed,
                last_access_ticks: 1,
            },
            CacheEntry {
                tier: CacheTier::Tier1Demand,
                last_access_ticks: 50,
            },
            CacheEntry {
                tier: CacheTier::Tier0Precache,
                last_access_ticks: 99,
            },
            CacheEntry {
                tier: CacheTier::Tier0Precache,
                last_access_ticks: 5,
            },
            CacheEntry {
                tier: CacheTier::Tier1Demand,
                last_access_ticks: 10,
            },
            CacheEntry {
                tier: CacheTier::Tier2Bribed,
                last_access_ticks: 100,
            },
        ];
        entries.sort_by_key(evict_key);

        let order: Vec<_> = entries
            .iter()
            .map(|e| (e.tier, e.last_access_ticks))
            .collect();
        assert_eq!(
            order,
            vec![
                (CacheTier::Tier0Precache, 5), // tier0 oldest first
                (CacheTier::Tier0Precache, 99),
                (CacheTier::Tier1Demand, 10), // then tier1 oldest first
                (CacheTier::Tier1Demand, 50),
                (CacheTier::Tier2Bribed, 1), // then tier2 oldest first
                (CacheTier::Tier2Bribed, 100),
            ]
        );
    }

    /// SPEC §2.1: precedence is by TIER ALONE. The freshest, most-recently-used tier0 entry is still
    /// evicted before the stalest tier1 one — recency cannot rescue a lower tier.
    #[test]
    fn a_fresh_low_tier_entry_is_evicted_before_a_stale_high_tier_one() {
        let fresh_precache = CacheEntry {
            tier: CacheTier::Tier0Precache,
            last_access_ticks: u64::MAX,
        };
        let stale_demand = CacheEntry {
            tier: CacheTier::Tier1Demand,
            last_access_ticks: 0,
        };
        assert!(evict_key(&fresh_precache) < evict_key(&stale_demand));
    }

    #[test]
    fn effective_tier_is_the_maximum_across_sources() {
        // Order of the sources must not matter — it is a max, not a last-writer-wins.
        assert_eq!(
            effective_tier([CacheTier::Tier0Precache, CacheTier::Tier1Demand]),
            Some(CacheTier::Tier1Demand)
        );
        assert_eq!(
            effective_tier([CacheTier::Tier1Demand, CacheTier::Tier0Precache]),
            Some(CacheTier::Tier1Demand)
        );
        assert_eq!(
            effective_tier([
                CacheTier::Tier0Precache,
                CacheTier::Tier2Bribed,
                CacheTier::Tier1Demand
            ]),
            Some(CacheTier::Tier2Bribed)
        );
    }

    /// SPEC §2.2: the promotion must survive the speculative reason lapsing. When the precache source
    /// stops naming the store, the demand claim alone still yields the promoted tier — the store does
    /// NOT fall back to `Tier0`.
    #[test]
    fn promotion_survives_the_speculative_source_lapsing() {
        let while_both = effective_tier([CacheTier::Tier0Precache, CacheTier::Tier1Demand]);
        let after_precache_lapses = effective_tier([CacheTier::Tier1Demand]);
        assert_eq!(while_both, Some(CacheTier::Tier1Demand));
        assert_eq!(after_precache_lapses, Some(CacheTier::Tier1Demand));
    }

    #[test]
    fn no_source_yields_none_and_callers_default_to_protected() {
        assert_eq!(effective_tier(std::iter::empty()), None);
        assert_eq!(DEFAULT_TIER, CacheTier::Tier1Demand);
    }
}
