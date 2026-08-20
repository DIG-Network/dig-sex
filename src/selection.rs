//! Selection — the objective function made executable (SPEC §0, §3.2, §4.1, §4.4).
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
//! higher-scoring large one within a tier** (SPEC §4.1). It never happens across tiers.
//!
//! # Incumbency is an input, because churn is an attack (SPEC §3.2, §8.5)
//!
//! Selection is run repeatedly against a drifting candidate set, so an implementation that compares
//! only scores will swap two near-equal stores for each other on every pass — spending this node's
//! disk bandwidth indefinitely and producing no net change in what is held. A peer that can influence
//! which candidates appear can drive that loop deliberately, which is why the SPEC calls the
//! displacement margin the **primary** defence against it rather than an efficiency tweak.
//!
//! The defence needs two things the objectives above cannot supply, and both are therefore carried by
//! the SIGNATURE rather than left to the caller's discipline:
//!
//! - **which candidates are already held** — [`SelectionCandidate::resident`]. Hysteresis is a
//!   comparison between an incumbent and a challenger, so a selection that cannot tell them apart
//!   cannot express it at all, whatever margin it is handed;
//! - **the margin itself** — a required part of [`SelectionPolicy`], defaulted to
//!   [`MIN_DISPLACEMENT_MARGIN`] and floored by [`DisplacementMargin`]. There is no constructor that
//!   omits it and no value that disables it, so the defence is **on for every caller** rather than
//!   available to the ones that go looking for it.
//!
//! The margin governs the **score** dimension, which is the one §3.2 names: a fresh candidate ranks
//! above an incumbent only when it exceeds the incumbent by the margin. It deliberately does NOT gate
//! the size dimension — a smaller fresh store displacing a larger incumbent increases the mirror
//! count, which is the objective being served, not churn.
//!
//! # Randomness is a network property, and it is seeded
//!
//! A deterministic final tiebreak makes every node with a similar view of the network choose the SAME
//! stores, so a handful are mirrored by everyone and the rest by nobody. Randomising decorrelates
//! independent nodes and is the only mechanism here that evens coverage without any node coordinating
//! with another (SPEC §4.4).
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

/// A node-local selection seed (SPEC §4.4).
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

/// The smallest displacement margin this crate will honour (SPEC §3.2, §8.5), and the default one.
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
/// This is the same rule [`select_within_capacity`] applies internally, exposed for a caller deciding
/// a single pairwise displacement outside a selection pass. Scores are only comparable within one
/// tier, so callers MUST NOT use this across tiers — cross-tier precedence is absolute and is decided
/// by the ladder, never by a score comparison (SPEC §2.1).
#[must_use]
pub fn may_displace(
    incumbent: RelevanceValue,
    candidate: RelevanceValue,
    margin: DisplacementMargin,
) -> bool {
    crate::relevance::should_displace(incumbent, candidate, margin.get())
}

/// Everything a selection pass needs beyond the candidates themselves (SPEC §9).
///
/// The knobs live in a struct rather than in the parameter list for one reason that the history of
/// this module makes concrete: the margin could not reach selection because the signature had nowhere
/// to put it, and a fourth positional argument would only defer the same problem to the fifth. Fields
/// are private and reached through [`Self::new`], so a future policy input is an additive method here
/// instead of another breaking signature change.
///
/// **The margin is not optional.** [`Self::new`] installs [`MIN_DISPLACEMENT_MARGIN`] — SPEC §9's
/// stated default — and [`Self::with_margin`] can only raise it, because [`DisplacementMargin`] floors
/// whatever it is given. A caller cannot construct a policy that selects without hysteresis.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SelectionPolicy {
    capacity_bytes: u64,
    seed: SelectionSeed,
    margin: DisplacementMargin,
}

impl SelectionPolicy {
    /// A policy over `capacity_bytes`, decorrelated by `seed`, with the displacement margin at its
    /// floor.
    #[must_use]
    pub fn new(capacity_bytes: u64, seed: SelectionSeed) -> Self {
        Self {
            capacity_bytes,
            seed,
            margin: DisplacementMargin::default(),
        }
    }

    /// Raise the displacement margin. A node that observes more churn than it likes buys stability
    /// here; it cannot buy the opposite, because the floor is applied in [`DisplacementMargin::new`].
    #[must_use]
    pub fn with_margin(self, margin: DisplacementMargin) -> Self {
        Self { margin, ..self }
    }

    /// The capacity this pass selects against.
    #[must_use]
    pub fn capacity_bytes(self) -> u64 {
        self.capacity_bytes
    }

    /// The node-local tiebreak seed.
    #[must_use]
    pub fn seed(self) -> SelectionSeed {
        self.seed
    }

    /// The effective displacement margin — never below [`MIN_DISPLACEMENT_MARGIN`].
    #[must_use]
    pub fn margin(self) -> DisplacementMargin {
        self.margin
    }
}

/// One candidate for retention, as selection sees it.
///
/// Generic over the identifier so the same selection serves both an in-memory candidate set (keyed by
/// content id) and the on-disk cache (keyed by `CapsuleIdentity`) without this crate re-declaring
/// either identity type (SPEC §11.3 — identifier types resolve to one version because this crate defines
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
    /// the operator's explicit override (SPEC §4.2).
    pub pinned: bool,
    /// Whether this candidate is **already held**. Incumbency, and the input the displacement margin
    /// is measured against (SPEC §3.2): a fresh candidate outranks an incumbent on score only by
    /// clearing the margin.
    ///
    /// It is a fact about this node's own disk, never a peer's claim. A caller that answers it
    /// `false` for everything gets a selection with no hysteresis at all, which is the churn loop
    /// §8.5 describes — so answer it from the cache, not from the candidate's provenance.
    pub resident: bool,
}

/// The outcome of a selection pass: what is retained and what is not, in decision order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection<Id> {
    /// Retained, in the order selected (highest tier first, then smallest first within a tier).
    pub retained: Vec<Id>,
    /// Not retained, in eviction order — the first entry is sacrificed first.
    pub rejected: Vec<Id>,
}

/// Select what to hold under `policy`, honouring profit first and mirror count second.
///
/// Pins are admitted unconditionally and consume budget first; if pins alone exceed capacity the
/// residual budget is zero and every unpinned candidate is rejected — the cache goes over capacity
/// rather than dropping a pin, which is the operator's stated intent.
///
/// Tiers are then offered the REMAINING capacity in descending rank, so a lower tier only ever sees
/// what the tiers above it did not claim. Within a tier, candidates are ordered smallest-first (the
/// count objective), then by score **discounted by the displacement margin for anything not already
/// held** (SPEC §3.2), then incumbent-before-challenger, then by a seeded shuffle (SPEC §4.4's last
/// step, reached only when every objective has run out of ordering power).
///
/// Rejected candidates are returned in eviction order: the lowest tier's rejects first, and within a
/// tier the ones selection reached last. Selection order reversed IS eviction order, which keeps the
/// two answers consistent by construction instead of by a second sort that could drift from this one.
///
/// # A NaN score is ordered, not rejected and not fatal
///
/// [`RelevanceValue`] wraps a public `f64`, so a consumer can construct a NaN score and reach this
/// path. Scores produced by [`crate::relevance::relevance`] are always finite, so a NaN is necessarily
/// caller-constructed — but "the caller should not do that" is not a contract, and this function is
/// reachable from peer-driven work.
///
/// The score comparison therefore uses [`f64::total_cmp`], which is a total order over **every** `f64`
/// including NaN. Consequences a caller can rely on:
///
/// - **it never panics on a NaN.** The obvious alternative, `partial_cmp(..).unwrap_or(Equal)`, is an
///   INCONSISTENT comparator whenever a NaN is present, and Rust's sort detects that and aborts the
///   process. That would make a value a consumer can construct into an availability failure of the
///   cache-selection path, which is a denial surface rather than an ordering artefact;
/// - **every candidate still lands in exactly one of `retained` or `rejected`**;
/// - a NaN-scored candidate sorts to one end (NaN orders outside every finite value under
///   `total_cmp`), so it is ranked, not dropped, and not treated as equal to everything.
///
/// For finite scores this is the same ordering as before, with one documented exception: `total_cmp`
/// orders `-0.0` before `+0.0` where `partial_cmp` calls them equal.
///
/// That pair can occur as a resident against a challenger, or between two challengers. A resident is
/// never the `-0.0` side — its contested score is `score + margin` with a positive margin, and
/// IEEE-754 gives `x + (-x) = +0.0` — but a challenger carries its score through unshifted, and
/// `RelevanceValue` wraps a public `f64`, so a challenger CAN be `-0.0`.
///
/// It changes no outcome either way. Against a challenger the resident's `+0.0` sorts first, which is
/// exactly where the incumbent-before-challenger tiebreak below already placed it — `total_cmp`
/// reaches that answer one tiebreak earlier. Between two zero-scored challengers it replaces what was
/// a seeded coin toss.
///
/// The tiebreak is what makes the first half of that true. Do not remove it.
#[must_use]
pub fn select_within_capacity<Id: Copy>(
    candidates: &[SelectionCandidate<Id>],
    policy: SelectionPolicy,
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
            .map(|(index, c)| (tiebreak(policy.seed, index), c))
            .collect();

        in_tier.sort_by(|(left_break, left), (right_break, right)| {
            left.size_bytes
                .cmp(&right.size_bytes)
                // `total_cmp`, not `partial_cmp(..).unwrap_or(Equal)`. The latter is an INCONSISTENT
                // comparator whenever a NaN is present, and Rust's sort detects that and panics —
                // turning a value a consumer can construct into an availability failure of the whole
                // cache-selection path. `total_cmp` is a total order over every f64 including NaN, so
                // the sort is always well-defined.
                .then_with(|| {
                    contested_score(right, policy.margin)
                        .total_cmp(&contested_score(left, policy.margin))
                })
                // The margin is a STRICT threshold, so a challenger that lands exactly on it has not
                // cleared it. Ordering incumbents first here is what keeps that boundary case a hold
                // rather than a coin toss decided by the shuffle below.
                .then_with(|| left.resident.cmp(&right.resident).reverse())
                .then_with(|| left_break.cmp(right_break))
        });

        for (_, candidate) in in_tier {
            let would_use = used.saturating_add(candidate.size_bytes);
            if would_use <= policy.capacity_bytes {
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

/// A candidate's score as the contest between incumbents and challengers sees it: face value for a
/// challenger, and face value **plus the margin** for something already held.
///
/// # Why the margin is added to the incumbent rather than subtracted from the challenger
///
/// Both express §3.2, and they are algebraically identical — but they are **not identical in
/// IEEE-754**, and the difference is a live defect rather than a nicety. Discounting the challenger
/// evaluates `challenger - margin > incumbent`, while [`may_displace`] and
/// [`crate::relevance::should_displace`] evaluate `challenger > incumbent + margin`. The two round
/// differently, and the discount form then displaces where the margin says hold — for example
/// `incumbent = 0.9048843869574763`, `margin = 0.1`, `challenger = incumbent + margin`, where
/// `should_displace` is `false` and `challenger - margin > incumbent` is `true`.
///
/// **How often depends entirely on the sample, so the rate is quoted with its method:** 124 of 512
/// exact-boundary triples (24%) with `margin = 0.1` over pseudo-random incumbents in `[0, 1)`, which
/// is the fixture `selection::tests` uses. A different margin or range gives a materially different
/// rate — single-digit percentages are easy to sample — so no single figure characterises the defect.
/// What matters is not the frequency but the DIRECTION: every divergence grants displacement.
///
/// That divergence **fails open**: it grants displacement, which is the direction an attacker working
/// the boundary wants, against the one defence §8.5 names as primary.
///
/// Adding the margin to the incumbent makes both sides evaluate the *same expression on the same
/// operands*, so the agreement with [`may_displace`] is exact **by construction** rather than by an
/// algebraic identity that floating point does not honour.
///
/// # What this preserves
///
/// Mapping every candidate through one function and then comparing normally is what keeps the
/// ordering total — the comparator never has to special-case a pair, so `sort_by` never sees an
/// intransitive answer for finite scores (see [`select_within_capacity`] on NaN).
///
/// Two incumbents, or two challengers, are shifted identically. Adding a constant is monotone
/// non-decreasing, so it can never invert their order; at most it merges two adjacent scores into a
/// tie, which then falls through to the tiebreaks below it.
fn contested_score<Id>(candidate: &SelectionCandidate<Id>, margin: DisplacementMargin) -> f64 {
    if candidate.resident {
        candidate.score.get() + margin.get()
    } else {
        candidate.score.get()
    }
}

/// The per-candidate shuffle value: the node-local seed mixed with the candidate's POSITION in the
/// input. Deliberately not mixed with the candidate's id — an id is peer-influenced, and mixing it in
/// would let a peer grind content that lands favourably in our tiebreaks (SPEC §4.4).
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A FRESH candidate — not currently held. The default for these fixtures because most of them
    /// exercise the count objective, which incumbency does not touch.
    fn candidate(id: u8, tier: CacheTier, size_bytes: u64, score: f64) -> SelectionCandidate<u8> {
        SelectionCandidate {
            id,
            tier,
            size_bytes,
            score: RelevanceValue(score),
            pinned: false,
            resident: false,
        }
    }

    /// The same candidate, already held.
    fn incumbent(id: u8, tier: CacheTier, size_bytes: u64, score: f64) -> SelectionCandidate<u8> {
        SelectionCandidate {
            resident: true,
            ..candidate(id, tier, size_bytes, score)
        }
    }

    fn seed() -> SelectionSeed {
        SelectionSeed::from_node_local(0x5E1E_C712_0000_0001)
    }

    /// A policy at the default (floored) margin.
    fn policy(capacity_bytes: u64) -> SelectionPolicy {
        SelectionPolicy::new(capacity_bytes, seed())
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
        let selection = select_within_capacity(&candidates, policy(100));

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
        let selection = select_within_capacity(&candidates, policy(100));

        assert_eq!(
            selection.retained,
            vec![b'P'],
            "the paid store claims capacity first and fifty free mirrors cannot outbid it"
        );
        assert_eq!(selection.rejected.len(), 50);
    }

    /// SPEC §4.1: within a tier, a lower-scoring SMALL store may be held over a higher-scoring large
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
            policy(50),
        );
        assert_eq!(within.retained, vec![b'S'], "small wins inside a tier");

        let across = select_within_capacity(
            &[
                candidate(b'S', CacheTier::Tier0Precache, 10, 1.0),
                candidate(b'B', CacheTier::Tier1Demand, 50, 9.0),
            ],
            policy(50),
        );
        assert_eq!(
            across.retained,
            vec![b'B'],
            "the higher tier claims capacity first, however small the lower-tier candidate is"
        );
    }

    /// SPEC §4.4: ties on profit AND size are broken randomly, so two nodes with different
    /// node-local seeds must not converge on the same stores. A deterministic tiebreak (input order,
    /// id order) returns the same answer for every seed and fails this.
    #[test]
    fn equal_profit_and_size_ties_are_decorrelated_across_nodes() {
        let candidates: Vec<_> = (0..8u8)
            .map(|id| candidate(id, CacheTier::Tier1Demand, 10, 1.0))
            .collect();

        let node_a = select_within_capacity(
            &candidates,
            SelectionPolicy::new(30, SelectionSeed::from_node_local(1)),
        );
        let node_b = select_within_capacity(
            &candidates,
            SelectionPolicy::new(30, SelectionSeed::from_node_local(2)),
        );

        assert_eq!(node_a.retained.len(), 3);
        assert_eq!(node_b.retained.len(), 3);
        assert_ne!(
            node_a.retained, node_b.retained,
            "independent nodes must not mirror the same three of eight identical stores"
        );
    }

    /// SPEC §4.4: randomness must not cost replayability. The SAME seed must reproduce the SAME
    /// selection exactly, or an eviction cannot be audited offline.
    #[test]
    fn the_same_seed_reproduces_the_same_selection() {
        let candidates: Vec<_> = (0..8u8)
            .map(|id| candidate(id, CacheTier::Tier1Demand, 10, 1.0))
            .collect();

        let first = select_within_capacity(
            &candidates,
            SelectionPolicy::new(30, SelectionSeed::from_node_local(7)),
        );
        let second = select_within_capacity(
            &candidates,
            SelectionPolicy::new(30, SelectionSeed::from_node_local(7)),
        );
        assert_eq!(first, second);
    }

    /// SPEC §4.4: randomness reaches ties only. Candidates that differ on size must order by size for
    /// EVERY seed — a shuffle applied before the objectives would break this for some seed.
    #[test]
    fn randomness_never_reaches_across_a_size_difference() {
        for raw_seed in 0..64u64 {
            let selection = select_within_capacity(
                &[
                    candidate(b'S', CacheTier::Tier1Demand, 10, 0.0),
                    candidate(b'B', CacheTier::Tier1Demand, 20, 100.0),
                ],
                SelectionPolicy::new(10, SelectionSeed::from_node_local(raw_seed)),
            );
            assert_eq!(
                selection.retained,
                vec![b'S'],
                "seed {raw_seed} moved selection across a size difference"
            );
        }
    }

    /// SPEC §4.2: a pin is retained even when it alone exceeds capacity, and it consumes the budget the
    /// rest of the selection is offered.
    #[test]
    fn a_pin_is_retained_over_capacity_and_consumes_the_budget() {
        let mut pinned = candidate(b'P', CacheTier::Tier0Precache, 500, 0.0);
        pinned.pinned = true;
        let selection = select_within_capacity(
            &[pinned, candidate(b'x', CacheTier::Tier1Demand, 1, 99.0)],
            policy(100),
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
            policy(100),
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

    // ---------------------------------------------------------------------------------------------
    // The margin, through the API a consumer actually calls (issue #4)
    //
    // Every fixture below is built so exactly ONE of two same-tier, same-SIZE candidates fits. Same
    // size is load-bearing: size is the primary sort, so any size difference would decide the outcome
    // before the score comparison the margin lives in was ever reached, and the test would pass
    // whether or not the margin was wired in at all.
    // ---------------------------------------------------------------------------------------------

    /// A challenger and an incumbent, alike in every respect the earlier objectives can see, with the
    /// challenger scoring higher by LESS than the margin. The incumbent is held.
    ///
    /// This is the assertion the whole change exists for: before it, `select_within_capacity` had no
    /// margin and no notion of incumbency, so it returned the challenger — the first half of a churn
    /// loop that repeats on every pass.
    #[test]
    fn a_challenger_inside_the_margin_does_not_displace_an_incumbent() {
        let selection = select_within_capacity(
            &[
                incumbent(b'i', CacheTier::Tier1Demand, 100, 1.0),
                candidate(
                    b'c',
                    CacheTier::Tier1Demand,
                    100,
                    1.0 + MIN_DISPLACEMENT_MARGIN / 2.0,
                ),
            ],
            policy(100),
        );

        assert_eq!(
            selection.retained,
            vec![b'i'],
            "a challenger that has not cleared the margin must not evict what is already held"
        );
        assert_eq!(selection.rejected, vec![b'c']);
    }

    /// The control the test above needs to mean anything: a challenger that DOES clear the margin
    /// displaces the incumbent. Without this, "always keep the incumbent" — hysteresis turned into a
    /// permanent freeze, which is a worse policy, not a better one — would pass.
    #[test]
    fn a_challenger_clearing_the_margin_does_displace_an_incumbent() {
        let selection = select_within_capacity(
            &[
                incumbent(b'i', CacheTier::Tier1Demand, 100, 1.0),
                candidate(
                    b'c',
                    CacheTier::Tier1Demand,
                    100,
                    1.0 + MIN_DISPLACEMENT_MARGIN * 2.0,
                ),
            ],
            policy(100),
        );

        assert_eq!(
            selection.retained,
            vec![b'c'],
            "a clearly better challenger must still be able to win, or the cache can never improve"
        );
    }

    /// A deterministic pseudo-random score in `[0, 1)`, mixed with the module's own finalizer so the
    /// fixture needs no dependency and replays exactly.
    ///
    /// **Irregular values are the point.** `1.0`, `0.5` and `1.0 + MIN_DISPLACEMENT_MARGIN` all round
    /// identically through both forms of the margin comparison, so a sweep built from round numbers
    /// cannot detect a rounding disagreement however many points it visits — which is exactly how an
    /// earlier version of these tests passed against a real fail-open defect.
    fn sampled_score(n: u64) -> f64 {
        mix64(n) as f64 / 18_446_744_073_709_551_616.0
    }

    /// The next representable `f64` above a positive finite value.
    fn next_up(x: f64) -> f64 {
        f64::from_bits(x.to_bits() + 1)
    }

    /// The next representable `f64` below a positive finite value.
    fn next_down(x: f64) -> f64 {
        f64::from_bits(x.to_bits() - 1)
    }

    /// The margin used by the boundary fixtures. `0.1` has no exact binary representation, so
    /// `incumbent + margin` is inexact for most incumbents — the condition under which two
    /// algebraically-equal comparisons diverge.
    const BOUNDARY_MARGIN: f64 = 0.1;

    /// Selection and [`may_displace`] must answer the SAME question the same way, **bit for bit** —
    /// two rules would drift, and a caller reasoning about one would be wrong about the other.
    ///
    /// Swept over ADVERSARIAL triples: pseudo-random incumbents, with challengers placed exactly ON
    /// `incumbent + margin` and one ULP either side of it. The exact-boundary and one-ULP-above cases
    /// pin the published bound from both sides, and the whole sweep sits where the two forms of the
    /// comparison round differently — the region a sweep of round numbers never reaches.
    #[test]
    fn selection_agrees_with_may_displace_at_adversarial_boundary_triples() {
        let margin = DisplacementMargin::new(BOUNDARY_MARGIN);

        for n in 0..512u64 {
            let incumbent_score = sampled_score(n);
            let boundary = incumbent_score + margin.get();

            for challenger_score in [next_down(boundary), boundary, next_up(boundary)] {
                let selection = select_within_capacity(
                    &[
                        incumbent(b'i', CacheTier::Tier1Demand, 100, incumbent_score),
                        candidate(b'c', CacheTier::Tier1Demand, 100, challenger_score),
                    ],
                    policy(100).with_margin(margin),
                );

                let pairwise = may_displace(
                    RelevanceValue(incumbent_score),
                    RelevanceValue(challenger_score),
                    margin,
                );
                assert_eq!(
                    selection.retained == vec![b'c'],
                    pairwise,
                    "sample {n}: selection and may_displace disagreed for incumbent \
                     {incumbent_score} against challenger {challenger_score}"
                );
            }
        }
    }

    /// A NaN score MUST NOT panic the selector, and every candidate MUST still land in exactly one of
    /// `retained` or `rejected`.
    ///
    /// `RelevanceValue` wraps a public `f64`, so a consumer can construct a NaN and reach this path.
    /// Under `partial_cmp(..).unwrap_or(Equal)` that produced an inconsistent comparator, which Rust's
    /// sort DETECTS and aborts on — *"user-provided comparison function does not correctly implement a
    /// total order"*. That is not a degraded ordering, it is an availability failure of the
    /// cache-selection path, reachable from a value a consumer can construct.
    ///
    /// The fixture is deliberately large and same-size: the sort only reaches the score comparison
    /// when size cannot separate candidates, and its total-order check needs enough elements to be
    /// exercised — a two-candidate fixture returns cleanly and proves nothing.
    #[test]
    fn a_nan_score_never_panics_the_selector_and_every_candidate_is_accounted_for() {
        let mut candidates: Vec<_> = (0..40u8)
            .map(|id| {
                candidate(
                    id,
                    CacheTier::Tier1Demand,
                    100,
                    sampled_score(u64::from(id)),
                )
            })
            .collect();
        for slot in [3usize, 17, 28, 39] {
            candidates[slot].score = RelevanceValue(f64::NAN);
        }
        candidates[11].resident = true;
        candidates[17].resident = true;

        // Room for half of them, so the pass genuinely partitions rather than retaining everything.
        let selection = select_within_capacity(&candidates, policy(2_000));

        let mut seen: Vec<u8> = selection
            .retained
            .iter()
            .chain(selection.rejected.iter())
            .copied()
            .collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(
            seen.len(),
            candidates.len(),
            "every candidate must appear exactly once across retained and rejected"
        );
        assert_eq!(
            selection.retained.len() + selection.rejected.len(),
            candidates.len(),
            "no candidate may be duplicated or dropped"
        );
    }

    /// Switching the comparator to `total_cmp` must not move any FINITE ordering, or the NaN fix
    /// would have bought availability at the cost of the selection this crate exists to make.
    ///
    /// This pins the exact deviation surface rather than asserting the absence of one: over sampled
    /// finite pairs the two comparators agree everywhere, and the ONLY finite disagreement possible is
    /// signed zero, which `partial_cmp` calls equal and `total_cmp` orders `-0.0 < +0.0`.
    #[test]
    fn total_cmp_agrees_with_partial_cmp_on_every_finite_score_except_signed_zero() {
        for n in 0..256u64 {
            let left = sampled_score(n) - 0.5;
            let right = sampled_score(n + 1) - 0.5;
            assert_eq!(
                left.total_cmp(&right),
                left.partial_cmp(&right).expect("both finite"),
                "comparators diverged on finite pair ({left}, {right})"
            );
        }

        // The one finite pair they treat differently, stated so it can never be a surprise.
        assert_eq!(
            (-0.0f64).partial_cmp(&0.0f64),
            Some(std::cmp::Ordering::Equal)
        );
        assert_eq!((-0.0f64).total_cmp(&0.0f64), std::cmp::Ordering::Less);
    }

    /// A resident's contested score can never BE `-0.0`.
    ///
    /// That is the whole of what this test proves, and it is narrower than it first appears: the
    /// signed-zero pair needs only ONE side to be `-0.0`, and a challenger carries its score through
    /// unshifted, so an incumbent-versus-challenger signed-zero comparison DOES arise. What makes it
    /// harmless is the incumbent-before-challenger tiebreak, not this property — the resident's
    /// `+0.0` sorts first under `total_cmp`, landing where that tiebreak already put it.
    ///
    /// `score + margin` with a margin at or above the floor yields `+0.0` even for the score that
    /// cancels it exactly, because IEEE-754 gives `x + (-x) = +0.0` under round-to-nearest.
    #[test]
    fn a_resident_contested_score_is_never_negative_zero() {
        let margin = DisplacementMargin::default();
        let cancelling = incumbent(b'z', CacheTier::Tier1Demand, 100, -margin.get());
        let contested = contested_score(&cancelling, margin);

        assert_eq!(contested, 0.0);
        assert!(
            contested.is_sign_positive(),
            "a cancelling resident score produced -0.0, which would expose the signed-zero ordering"
        );
    }

    /// The guard that keeps the test above honest: it asserts the sampled set actually CONTAINS
    /// triples that distinguish the two forms of the margin comparison.
    ///
    /// `challenger - margin > incumbent` and `challenger > incumbent + margin` are algebraically
    /// identical and NOT identical in IEEE-754. A fixture on which they never disagree cannot detect
    /// a selection that uses the wrong one, so without this assertion the sweep above could silently
    /// degenerate back into the benign fixture it replaced — passing while the defence fails open.
    #[test]
    fn the_boundary_fixture_actually_distinguishes_the_two_margin_forms() {
        let margin = BOUNDARY_MARGIN;

        let divergent = (0..512u64)
            .filter(|&n| {
                let incumbent = sampled_score(n);
                let challenger = incumbent + margin;
                let discount_form = challenger - margin > incumbent;
                let addition_form = challenger > incumbent + margin;
                discount_form != addition_form
            })
            .count();

        assert!(
            divergent > 0,
            "the boundary fixture no longer contains a triple on which the two forms of the margin \
             comparison disagree, so it can no longer detect the wrong one"
        );
    }

    /// The margin is a policy input, not a constant: raising it widens the band a challenger must
    /// clear. A fixture whose challenger clears the FLOOR but not the raised margin distinguishes a
    /// wired-in margin from one hardcoded to `MIN_DISPLACEMENT_MARGIN`.
    #[test]
    fn a_raised_margin_widens_the_band_selection_enforces() {
        let contenders = [
            incumbent(b'i', CacheTier::Tier1Demand, 100, 1.0),
            candidate(b'c', CacheTier::Tier1Demand, 100, 1.2),
        ];

        assert_eq!(
            select_within_capacity(&contenders, policy(100)).retained,
            vec![b'c'],
            "at the floor, a challenger 0.2 ahead clears it"
        );
        assert_eq!(
            select_within_capacity(
                &contenders,
                policy(100).with_margin(DisplacementMargin::new(0.5))
            )
            .retained,
            vec![b'i'],
            "at a 0.5 margin the same challenger does not"
        );
    }

    /// SPEC §8.5: the defence must be unbypassable through the path a consumer configures, not only
    /// through [`DisplacementMargin::new`]. A policy asked for no margin at all still refuses a
    /// marginal displacement.
    #[test]
    fn a_policy_cannot_be_configured_to_select_without_hysteresis() {
        let selection = select_within_capacity(
            &[
                incumbent(b'i', CacheTier::Tier1Demand, 100, 1.0),
                candidate(
                    b'c',
                    CacheTier::Tier1Demand,
                    100,
                    1.0 + MIN_DISPLACEMENT_MARGIN / 2.0,
                ),
            ],
            policy(100).with_margin(DisplacementMargin::new(0.0)),
        );

        assert_eq!(selection.retained, vec![b'i']);
        assert_eq!(
            SelectionPolicy::new(100, seed()).margin().get(),
            MIN_DISPLACEMENT_MARGIN,
            "the default policy margin is SPEC 9's stated default, not zero"
        );
    }

    /// Incumbency is subordinate to the tier ladder, exactly as score is (SPEC §2.1). A resident
    /// `Tier0` entry does not survive a fresh `Tier1` one — profit is absolute and hysteresis is a
    /// within-tier rule, so an implementation that checked residency before the tier would invert
    /// this.
    #[test]
    fn incumbency_never_outranks_the_tier_ladder() {
        let selection = select_within_capacity(
            &[
                incumbent(b'i', CacheTier::Tier0Precache, 100, 100.0),
                candidate(b'c', CacheTier::Tier1Demand, 100, 0.0),
            ],
            policy(100),
        );

        assert_eq!(
            selection.retained,
            vec![b'c'],
            "a higher tier claims capacity ahead of a lower-tier incumbent, whatever it scores"
        );
    }

    /// The margin governs the SCORE dimension only (SPEC §3.2). A smaller challenger still displaces
    /// a larger incumbent, because that raises the mirror count — the objective, not churn. An
    /// implementation that applied hysteresis to the size comparison too would freeze the cache in
    /// whatever shape it first reached.
    #[test]
    fn hysteresis_does_not_block_a_challenger_that_improves_the_mirror_count() {
        let selection = select_within_capacity(
            &[
                incumbent(b'i', CacheTier::Tier1Demand, 100, 100.0),
                candidate(b'c', CacheTier::Tier1Demand, 50, 0.0),
                candidate(b'd', CacheTier::Tier1Demand, 50, 0.0),
            ],
            policy(100),
        );

        assert_eq!(
            selection.retained.len(),
            2,
            "two smaller challengers replace one larger incumbent: that is the count objective"
        );
        assert!(!selection.retained.contains(&b'i'));
    }

    /// Two incumbents are discounted identically, so they compare on face value — the margin decides
    /// displacement, it does not reorder the cache against itself. An implementation that discounted
    /// by residency in some other arrangement (or applied the margin unconditionally) would keep the
    /// lower-scoring one here.
    #[test]
    fn two_incumbents_are_ordered_by_score_alone() {
        let selection = select_within_capacity(
            &[
                incumbent(b'l', CacheTier::Tier1Demand, 100, 1.0),
                incumbent(
                    b'h',
                    CacheTier::Tier1Demand,
                    100,
                    1.0 + MIN_DISPLACEMENT_MARGIN / 2.0,
                ),
            ],
            policy(100),
        );

        assert_eq!(
            selection.retained,
            vec![b'h'],
            "between two things already held there is nothing to displace, so score decides"
        );
    }

    /// Two challengers are likewise discounted identically. Neither is held, so neither has
    /// incumbency to protect and the margin must not appear between them.
    #[test]
    fn two_challengers_are_ordered_by_score_alone() {
        let selection = select_within_capacity(
            &[
                candidate(b'l', CacheTier::Tier1Demand, 100, 1.0),
                candidate(
                    b'h',
                    CacheTier::Tier1Demand,
                    100,
                    1.0 + MIN_DISPLACEMENT_MARGIN / 2.0,
                ),
            ],
            policy(100),
        );

        assert_eq!(selection.retained, vec![b'h']);
    }

    /// SPEC §3.2's strict threshold: a challenger landing EXACTLY on `incumbent + margin` has not
    /// cleared it.
    ///
    /// Two dimensions are swept together because each hides a different defect, and a fixture
    /// covering one while degenerate in the other passes against a real bug:
    ///
    /// - **seeds**, because at the exact boundary the two contested scores are equal and the seeded
    ///   shuffle decides. An implementation missing the incumbent-first tiebreak fails for only some
    ///   seeds;
    /// - **scores**, because `1.0 + MIN_DISPLACEMENT_MARGIN` rounds identically under both forms of
    ///   the margin comparison. Against a fail-open discount form, that single benign value passes
    ///   for every seed — which it did.
    #[test]
    fn a_challenger_exactly_on_the_margin_loses_for_every_seed_and_every_sampled_score() {
        let margin = DisplacementMargin::new(BOUNDARY_MARGIN);

        for raw_seed in 0..64u64 {
            for n in 0..16u64 {
                let incumbent_score = sampled_score(n);
                let selection = select_within_capacity(
                    &[
                        incumbent(b'i', CacheTier::Tier1Demand, 100, incumbent_score),
                        candidate(
                            b'c',
                            CacheTier::Tier1Demand,
                            100,
                            incumbent_score + margin.get(),
                        ),
                    ],
                    SelectionPolicy::new(100, SelectionSeed::from_node_local(raw_seed))
                        .with_margin(margin),
                );
                assert_eq!(
                    selection.retained,
                    vec![b'i'],
                    "seed {raw_seed}, incumbent {incumbent_score}: a challenger exactly ON the \
                     margin displaced an incumbent"
                );
            }
        }
    }

    /// The churn loop itself, run as a loop: a cache holding one of two near-equal stores, offered
    /// the other on every pass, must settle. Two passes are what a single displacement test cannot
    /// show — the defect is not one wrong choice, it is that the choice keeps reversing.
    #[test]
    fn repeated_passes_over_near_equal_candidates_settle_instead_of_churning() {
        let mut held = b'i';
        for pass in 0..8 {
            let other = if held == b'i' { b'c' } else { b'i' };
            let contenders = [
                incumbent(held, CacheTier::Tier1Demand, 100, 1.0),
                candidate(
                    other,
                    CacheTier::Tier1Demand,
                    100,
                    1.0 + MIN_DISPLACEMENT_MARGIN / 2.0,
                ),
            ];
            let retained = select_within_capacity(&contenders, policy(100)).retained;
            assert_eq!(
                retained,
                vec![b'i'],
                "pass {pass} swapped the held store for a near-equal one"
            );
            held = retained[0];
        }
    }
}
