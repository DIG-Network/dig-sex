//! Selection — the objective function made executable (SPEC §0, §3.1, §3.2).
//!
//! # The objective, in the order it is applied
//!
//! **Profit first; subject to that, maximise the NUMBER of mirrors within the allocation.** The two
//! are **lexicographic, not weighted**: there is no exchange rate between them and this module
//! deliberately provides no way to express one. Profit is carried entirely by the
//! [`CacheTier`](crate::tier::CacheTier) ladder — a higher tier claims capacity before a lower tier
//! is offered any — so no quantity of small stores can ever displace paid content.
//!
//! Within one tier, the objective is a **count**: mirrors are units of network usefulness regardless
//! of size, so many small stores beat one large store. That is why [`relevance`](crate::relevance)
//! deliberately keeps `size_bytes` OUT of the score. Score is the **value**, size is the **weight**,
//! and this module is the selection layer the existing model reserved that field for.
//!
//! # Why the ordering is size-before-score, and why that is not a bug
//!
//! Sorting by score and filling until full is the obvious implementation and it is the wrong one: it
//! optimises retained relevance, which is not the objective. Under a single capacity bound the number
//! of items retained is maximised by taking the SMALLEST first, so that is the primary sort. Score
//! then chooses among candidates the count objective cannot separate.
//!
//! The visible consequence is intended: **a lower-scoring small store may be held over a
//! higher-scoring large one within a tier** (SPEC §3.1). It never happens across tiers.
//!
//! # Randomness is a network property, and it is seeded
//!
//! A deterministic final tiebreak makes every node with a similar view of the network choose the SAME
//! stores, so a handful are mirrored by everyone and the rest by nobody. Randomising decorrelates
//! independent nodes and is the only mechanism here that evens coverage without any node coordinating
//! with another (SPEC §3.2).
//!
//! Two constraints keep that compatible with the replayability the whole model rests on, and both are
//! enforced by the SIGNATURE rather than by convention:
//!
//! - the seed is an **input** ([`SelectionSeed`]), never drawn ambiently — so the same inputs,
//!   including the seed, always reproduce the same selection and any decision is auditable offline;
//! - the seed MUST NOT be derivable from peer-supplied input. The tiebreak mixes the seed with a
//!   candidate's POSITION in the input, never with its content id or provider count, so nothing a
//!   peer supplies reaches the shuffle. An attacker who could predict this seed could bias which ties
//!   this node resolves in their favour, turning decorrelation into targeting.

use crate::relevance::RelevanceValue;
use crate::tier::CacheTier;

/// A node-local selection seed (SPEC §3.2).
///
/// Construct it from the node's own identity or local entropy. **Never** from a content id, a
/// provider count, or anything else a peer supplies: a peer-derivable seed lets an attacker predict
/// this node's tiebreaks and steer them.
///
/// It is a distinct type rather than a bare `u64` so a peer-supplied value cannot be passed by
/// accident — the conversion is explicit and named, and reads as a claim at every call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionSeed(u64);

impl SelectionSeed {
    /// Build a seed from node-local state. The caller asserts, by calling this, that `value` is not
    /// derivable from peer-supplied input.
    #[must_use]
    pub const fn from_node_local(value: u64) -> Self {
        Self(value)
    }

    /// Build a seed from this node's own peer id — the canonical node-local, non-peer-derivable
    /// source. A peer cannot choose OUR identity, so it cannot predict the resulting tiebreaks
    /// without already knowing us.
    #[must_use]
    pub fn from_peer_id(peer_id: &[u8; 32]) -> Self {
        let mut value = 0u64;
        for byte in peer_id.iter().take(8) {
            value = (value << 8) | u64::from(*byte);
        }
        Self(mix64(value))
    }
}

/// One candidate for retention, as selection sees it.
///
/// Generic over the identifier so the same selection serves both an in-memory candidate set (keyed by
/// content id) and the on-disk cache (keyed by `CapsuleIdentity`) without this crate re-declaring
/// either identity type (SPEC §9 — identifier types resolve to one version because this crate defines
/// none of its own).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SelectionCandidate<Id> {
    /// What is being selected.
    pub id: Id,
    /// Its effective tier — the profit dimension, and an absolute bound on what it can displace.
    pub tier: CacheTier,
    /// On-disk weight in bytes. The count objective's cost term.
    pub size_bytes: u64,
    /// Its relevance score WITHIN its tier. Never compared across tiers.
    pub score: RelevanceValue,
    /// An operator pin. A pinned candidate is always retained and MAY push the node over capacity —
    /// the operator's explicit override (SPEC §3).
    pub pinned: bool,
}

/// The outcome of a selection pass: what is retained and what is not, in decision order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection<Id> {
    /// Retained, in the order selected (highest tier first, then smallest first within a tier).
    pub retained: Vec<Id>,
    /// Not retained, in eviction order — the first entry is sacrificed first.
    pub rejected: Vec<Id>,
}

/// Select what to hold within `capacity_bytes`, honouring profit first and mirror count second.
///
/// Pins are admitted unconditionally and consume budget first; if pins alone exceed capacity the
/// residual budget is zero and every unpinned candidate is rejected — the cache goes over capacity
/// rather than dropping a pin, which is the operator's stated intent.
///
/// Tiers are then offered the REMAINING capacity in descending rank, so a lower tier only ever sees
/// what the tiers above it did not claim. Within a tier, candidates are ordered smallest-first (the
/// count objective), then by score descending (the value the count objective cannot separate), then
/// by a seeded shuffle (SPEC §3.2's last step, reached only when every objective has run out of
/// ordering power).
///
/// Rejected candidates are returned in eviction order: the lowest tier's rejects first, and within a
/// tier the ones selection reached last. Selection order reversed IS eviction order, which keeps the
/// two answers consistent by construction instead of by a second sort that could drift from this one.
#[must_use]
pub fn select_within_capacity<Id: Copy>(
    candidates: &[SelectionCandidate<Id>],
    capacity_bytes: u64,
    seed: SelectionSeed,
) -> Selection<Id> {
    let mut retained = Vec::new();
    let mut rejected = Vec::new();
    let mut used = 0u64;

    for pinned in candidates.iter().filter(|c| c.pinned) {
        retained.push(pinned.id);
        used = used.saturating_add(pinned.size_bytes);
    }

    // Descending tier rank: the profit objective claims capacity before the count objective sees any.
    for tier in [
        CacheTier::Tier2Bribed,
        CacheTier::Tier1Demand,
        CacheTier::Tier0Precache,
    ] {
        let mut in_tier: Vec<(u64, &SelectionCandidate<Id>)> = candidates
            .iter()
            .enumerate()
            .filter(|(_, c)| c.tier == tier && !c.pinned)
            .map(|(index, c)| (tiebreak(seed, index), c))
            .collect();

        in_tier.sort_by(|(left_break, left), (right_break, right)| {
            left.size_bytes
                .cmp(&right.size_bytes)
                .then_with(|| {
                    right
                        .score
                        .get()
                        .partial_cmp(&left.score.get())
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| left_break.cmp(right_break))
        });

        for (_, candidate) in in_tier {
            let would_use = used.saturating_add(candidate.size_bytes);
            if would_use <= capacity_bytes {
                used = would_use;
                retained.push(candidate.id);
            } else {
                rejected.push(candidate.id);
            }
        }
    }

    // Selection ran highest tier first; eviction runs lowest tier first.
    rejected.reverse();
    Selection { retained, rejected }
}

/// The per-candidate shuffle value: the node-local seed mixed with the candidate's POSITION in the
/// input. Deliberately not mixed with the candidate's id — an id is peer-influenced, and mixing it in
/// would let a peer grind content that lands favourably in our tiebreaks (SPEC §3.2).
fn tiebreak(seed: SelectionSeed, index: usize) -> u64 {
    mix64(seed.0 ^ mix64(index as u64))
}

/// SplitMix64's finalizer — a well-known avalanche mix. Used instead of a `rand` dependency because
/// the requirement here is a DETERMINISTIC, replayable permutation from an explicit seed, which is
/// exactly what a mixing function gives and what a generator carrying hidden state does not.
const fn mix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// The smallest displacement margin this crate will honour (SPEC §3.2, §8.5).
///
/// **A zero margin is a denial-of-service vector, not merely an inefficiency.** Without a margin, two
/// near-equal candidates displace each other on every sweep, and a peer able to drive admission can
/// spend this node's disk bandwidth indefinitely while producing no net change in what is held.
pub const MIN_DISPLACEMENT_MARGIN: f64 = 0.01;

/// A displacement margin, floored so it can never be configured to zero.
///
/// The floor is applied in the CONSTRUCTOR rather than at the comparison, so there is no way to hold a
/// zero-margin value at all — a check at the point of use can be bypassed by a second call site, and
/// this rule has to hold for every one of them.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct DisplacementMargin(f64);

impl DisplacementMargin {
    /// Build a margin, raising anything below [`MIN_DISPLACEMENT_MARGIN`] (including zero, a negative
    /// value, or NaN) to the floor.
    #[must_use]
    pub fn new(requested: f64) -> Self {
        if requested.is_finite() && requested > MIN_DISPLACEMENT_MARGIN {
            Self(requested)
        } else {
            Self(MIN_DISPLACEMENT_MARGIN)
        }
    }

    /// The effective margin.
    #[must_use]
    pub fn get(self) -> f64 {
        self.0
    }
}

impl Default for DisplacementMargin {
    fn default() -> Self {
        Self::new(MIN_DISPLACEMENT_MARGIN)
    }
}

/// Whether `candidate` may displace `incumbent` from a full cache, under an unbypassable margin.
///
/// Wraps [`crate::relevance::should_displace`] so no call site can supply a raw zero. Scores are only
/// comparable within one tier, so callers MUST NOT use this across tiers — cross-tier precedence is
/// absolute and is decided by the ladder, never by a score comparison (SPEC §2.1).
#[must_use]
pub fn may_displace(
    incumbent: RelevanceValue,
    candidate: RelevanceValue,
    margin: DisplacementMargin,
) -> bool {
    crate::relevance::should_displace(incumbent, candidate, margin.get())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: u8, tier: CacheTier, size_bytes: u64, score: f64) -> SelectionCandidate<u8> {
        SelectionCandidate {
            id,
            tier,
            size_bytes,
            score: RelevanceValue(score),
            pinned: false,
        }
    }

    fn seed() -> SelectionSeed {
        SelectionSeed::from_node_local(0x5E1E_C712_0000_0001)
    }

    /// SPEC §0/§0.1: the secondary objective is a COUNT. Two small stores beat one large store even
    /// when the large one scores higher — the fixture is built so "sort by score and fill", the
    /// obvious wrong implementation, retains exactly ONE store and this assertion fails.
    #[test]
    fn many_small_stores_beat_one_large_higher_scoring_store() {
        let candidates = [
            candidate(b'L', CacheTier::Tier1Demand, 100, 10.0),
            candidate(b'a', CacheTier::Tier1Demand, 40, 1.0),
            candidate(b'b', CacheTier::Tier1Demand, 40, 1.0),
        ];
        let selection = select_within_capacity(&candidates, 100, seed());

        assert_eq!(
            selection.retained.len(),
            2,
            "count objective must retain two small stores, not the one high-scoring large one"
        );
        assert!(!selection.retained.contains(&b'L'));
        assert_eq!(selection.rejected, vec![b'L']);
    }

    /// SPEC §0/§2.1: lexicographic, no exchange rate. The fixture offers the count objective its
    /// best possible bribe — MANY tiny high-scoring `Tier1` stores against ONE large `Tier2` one —
    /// and paid retention must still win outright. A weighted objective retains the small set.
    #[test]
    fn no_quantity_of_mirrors_displaces_the_paid_tier() {
        let mut candidates = vec![candidate(b'P', CacheTier::Tier2Bribed, 100, 0.1)];
        for id in 0..50u8 {
            candidates.push(candidate(id, CacheTier::Tier1Demand, 2, 100.0));
        }
        let selection = select_within_capacity(&candidates, 100, seed());

        assert_eq!(
            selection.retained,
            vec![b'P'],
            "the paid store claims capacity first and fifty free mirrors cannot outbid it"
        );
        assert_eq!(selection.rejected.len(), 50);
    }

    /// SPEC §3.1: within a tier, a lower-scoring SMALL store may be held over a higher-scoring large
    /// one. Across tiers it must not — so the same size/score pairing placed in different tiers
    /// inverts the outcome. Testing only the within-tier half would pass for an implementation that
    /// ignored tiers entirely.
    #[test]
    fn size_beats_score_within_a_tier_but_never_across_tiers() {
        let within = select_within_capacity(
            &[
                candidate(b'S', CacheTier::Tier1Demand, 10, 1.0),
                candidate(b'B', CacheTier::Tier1Demand, 90, 9.0),
            ],
            50,
            seed(),
        );
        assert_eq!(within.retained, vec![b'S'], "small wins inside a tier");

        let across = select_within_capacity(
            &[
                candidate(b'S', CacheTier::Tier0Precache, 10, 1.0),
                candidate(b'B', CacheTier::Tier1Demand, 50, 9.0),
            ],
            50,
            seed(),
        );
        assert_eq!(
            across.retained,
            vec![b'B'],
            "the higher tier claims capacity first, however small the lower-tier candidate is"
        );
    }

    /// SPEC §3.2: ties on profit AND size are broken randomly, so two nodes with different
    /// node-local seeds must not converge on the same stores. A deterministic tiebreak (input order,
    /// id order) returns the same answer for every seed and fails this.
    #[test]
    fn equal_profit_and_size_ties_are_decorrelated_across_nodes() {
        let candidates: Vec<_> = (0..8u8)
            .map(|id| candidate(id, CacheTier::Tier1Demand, 10, 1.0))
            .collect();

        let node_a = select_within_capacity(&candidates, 30, SelectionSeed::from_node_local(1));
        let node_b = select_within_capacity(&candidates, 30, SelectionSeed::from_node_local(2));

        assert_eq!(node_a.retained.len(), 3);
        assert_eq!(node_b.retained.len(), 3);
        assert_ne!(
            node_a.retained, node_b.retained,
            "independent nodes must not mirror the same three of eight identical stores"
        );
    }

    /// SPEC §3/§3.2: randomness must not cost replayability. The SAME seed must reproduce the SAME
    /// selection exactly, or an eviction cannot be audited offline.
    #[test]
    fn the_same_seed_reproduces_the_same_selection() {
        let candidates: Vec<_> = (0..8u8)
            .map(|id| candidate(id, CacheTier::Tier1Demand, 10, 1.0))
            .collect();

        let first = select_within_capacity(&candidates, 30, SelectionSeed::from_node_local(7));
        let second = select_within_capacity(&candidates, 30, SelectionSeed::from_node_local(7));
        assert_eq!(first, second);
    }

    /// SPEC §3.2: randomness reaches ties only. Candidates that differ on size must order by size for
    /// EVERY seed — a shuffle applied before the objectives would break this for some seed.
    #[test]
    fn randomness_never_reaches_across_a_size_difference() {
        for raw_seed in 0..64u64 {
            let selection = select_within_capacity(
                &[
                    candidate(b'S', CacheTier::Tier1Demand, 10, 0.0),
                    candidate(b'B', CacheTier::Tier1Demand, 20, 100.0),
                ],
                10,
                SelectionSeed::from_node_local(raw_seed),
            );
            assert_eq!(
                selection.retained,
                vec![b'S'],
                "seed {raw_seed} moved selection across a size difference"
            );
        }
    }

    /// SPEC §3: a pin is retained even when it alone exceeds capacity, and it consumes the budget the
    /// rest of the selection is offered.
    #[test]
    fn a_pin_is_retained_over_capacity_and_consumes_the_budget() {
        let mut pinned = candidate(b'P', CacheTier::Tier0Precache, 500, 0.0);
        pinned.pinned = true;
        let selection = select_within_capacity(
            &[pinned, candidate(b'x', CacheTier::Tier1Demand, 1, 99.0)],
            100,
            seed(),
        );

        assert!(selection.retained.contains(&b'P'), "a pin is never dropped");
        assert_eq!(
            selection.rejected,
            vec![b'x'],
            "the pin's bytes leave no residual capacity, even for a higher tier"
        );
    }

    /// Rejected entries come back in EVICTION order — lowest tier first — so a caller can hand the
    /// list straight to eviction without re-sorting it (and without that second sort drifting from
    /// this one).
    #[test]
    fn rejects_are_returned_in_eviction_order_lowest_tier_first() {
        let selection = select_within_capacity(
            &[
                candidate(b'0', CacheTier::Tier0Precache, 100, 0.0),
                candidate(b'1', CacheTier::Tier1Demand, 100, 0.0),
                candidate(b'2', CacheTier::Tier2Bribed, 100, 0.0),
            ],
            100,
            seed(),
        );

        assert_eq!(selection.retained, vec![b'2']);
        assert_eq!(
            selection.rejected,
            vec![b'0', b'1'],
            "tier0 is sacrificed before tier1"
        );
    }

    /// The seed derived from this node's peer id must actually depend on it — a constant derivation
    /// would silently re-correlate every node in the network.
    #[test]
    fn peer_id_seeds_differ_between_nodes() {
        assert_ne!(
            SelectionSeed::from_peer_id(&[1u8; 32]),
            SelectionSeed::from_peer_id(&[2u8; 32])
        );
    }

    /// SPEC §3.2/§8.5: the margin can never be zero, however it is configured. Zero, negative and NaN
    /// all raise to the floor — a check written only against `0.0` misses the other two, and each is a
    /// value a configuration file can actually produce.
    #[test]
    fn the_displacement_margin_can_never_be_configured_to_zero() {
        for requested in [0.0, -1.0, f64::NAN, f64::NEG_INFINITY] {
            assert_eq!(
                DisplacementMargin::new(requested).get(),
                MIN_DISPLACEMENT_MARGIN,
                "{requested} must raise to the floor"
            );
        }
        assert!(DisplacementMargin::default().get() > 0.0);
    }

    /// A margin above the floor is honoured — the control. Without it, an implementation that always
    /// returned the floor would pass the test above.
    #[test]
    fn a_margin_above_the_floor_is_honoured() {
        assert_eq!(DisplacementMargin::new(0.5).get(), 0.5);
    }

    /// The floored margin is what actually gates displacement: a candidate inside the band stays out
    /// even when the caller asked for no margin at all, which is the churn loop this prevents.
    #[test]
    fn a_zero_margin_request_still_blocks_a_marginal_displacement() {
        let margin = DisplacementMargin::new(0.0);
        assert!(!may_displace(
            RelevanceValue(1.0),
            RelevanceValue(1.0 + MIN_DISPLACEMENT_MARGIN / 2.0),
            margin
        ));
        assert!(may_displace(
            RelevanceValue(1.0),
            RelevanceValue(1.0 + MIN_DISPLACEMENT_MARGIN * 2.0),
            margin
        ));
    }
}
