//! Ranking the peers a recursive ask is forwarded to (SPEC §6.1.2).
//!
//! [`decide_forward`](crate::discovery::decide_forward) has to choose `fan_out` peers out of the
//! connected pool. Choosing them by taking a prefix of the caller's slice is not a neutral default:
//! the pool reaches this crate from a `HashMap`, whose iteration order is arbitrary **but stable for
//! a given map instance**, so a prefix is not a random sample — it is a *fixed arbitrary* one. For the
//! lifetime of a process the same handful of peers absorb every forwarded ask and the rest of the
//! pool is never asked at all. That is a routing-quality defect and a load-concentration defect at
//! once.
//!
//! This module replaces the prefix with an order derived from what **this node has observed**.
//!
//! # OBSERVED ONLY, and that is enforced by the type
//!
//! [`PeerObservations`] can be reached in exactly two ways: [`PeerObservations::unobserved`], and
//! [`AskObservations::record`], which folds in the outcome of an exchange **this node completed**.
//! Its fields are private, it derives no deserialization, and there is deliberately **no conversion
//! from any wire-shaped value**. So there is no channel at all by which a peer can assert its own
//! quality, and the eclipse attack against a novelty-seeking router is not mitigated here — it is
//! unreachable. Adding a `From<_>` for a decoded message would remove that property silently, which is
//! why the absence is stated rather than assumed.
//!
//! Nothing here is emitted, gossiped, or persisted across a restart: this is a node-local view, in the
//! same sense and for the same reason as [`crate::conduct`] (SPEC §8.2A — reputation is local and is
//! never gossiped as fact).
//!
//! # Recency decays toward the UNOBSERVED baseline, not toward zero
//!
//! A score that decayed toward zero would turn an old penalty into a permanent exclusion, and a peer
//! that is never asked can never demonstrate recovery. Every score therefore relaxes toward
//! [`UNOBSERVED_QUALITY`] — the score an unobserved peer already has — so stale praise and stale blame
//! both expire into "unknown".
//!
//! # Ties break on IDENTITY, deliberately unlike [`crate::selection`]
//!
//! `selection` mixes its seed with a candidate's *position* precisely because a content id is
//! peer-influenced. Here position is the very thing that must not be honoured — it *is* the `HashMap`
//! order — so the tiebreak mixes the node-local [`SelectionSeed`] with the peer's routing identity
//! instead. That identity is the verified mTLS session key hash, so influencing it costs a keypair AND
//! admission to this node's connected pool, and the outcome still cannot be predicted without the
//! node-local seed.
//!
//! # Bounds
//!
//! Observations are keyed only by peers presently in the connected pool and are dropped when the pool
//! drops them ([`AskObservations::retain`]), so there is no map keyed by untrusted input. A capacity
//! ceiling with least-recently-observed eviction is applied anyway, so the bound survives a caller
//! that forgets to call `retain` (SPEC §8.4).

use crate::selection::SelectionSeed;

/// The quality score of a peer this node has never observed answering an ask.
///
/// Mid-scale on purpose. An unobserved peer must outrank a peer observed to be bad and be outranked
/// by one observed to be good; anchoring it at either end collapses one of those two orderings.
pub const UNOBSERVED_QUALITY: f64 = 0.5;

/// Ticks after which the weight carried by an observation halves.
pub const QUALITY_HALF_LIFE_TICKS: u64 = 900;

/// The latency, in ticks, at which the speed component of a score is halved. A scale rather than a
/// deadline: a slow answer is worth less than a fast one, never nothing.
pub const LATENCY_SCALE_TICKS: u64 = 4;

/// The default ceiling on how many peers are tracked. Sized to a generous connected pool, since the
/// pool is the real bound and this is defence in depth.
pub const DEFAULT_OBSERVATION_CAPACITY: usize = 256;

/// How a peer answered one forwarded ask, as observed by **this node**, from its own exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskOutcome {
    /// Answered with at least one usable provider. The outcome routing exists to find.
    Conclusive,
    /// Answered, but with nothing usable — an honest absence, or hearsay that led nowhere.
    ///
    /// Ranked above [`AskOutcome::Silent`] so that answering is never worse than not answering
    /// (SPEC §8.2A).
    Inconclusive,
    /// Did not answer within the caller's own deadline. Not verifiable, and manufacturable in a
    /// third party, so it is scored gently and decays like everything else.
    Silent,
}

/// What this node has observed of one peer's answers.
///
/// Fields are private and there is no constructor from anything a peer supplies; see the module docs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PeerObservations {
    asks: u32,
    answered: u32,
    conclusive: u32,
    latency_ticks_total: u64,
    last_observed_ticks: u64,
}

impl PeerObservations {
    /// The state of a peer this node has never observed.
    #[must_use]
    pub const fn unobserved() -> Self {
        Self {
            asks: 0,
            answered: 0,
            conclusive: 0,
            latency_ticks_total: 0,
            last_observed_ticks: 0,
        }
    }

    /// Whether this node has ever observed this peer answer an ask.
    #[must_use]
    pub const fn is_unobserved(&self) -> bool {
        self.asks == 0
    }

    /// How many completed exchanges this node has folded in.
    #[must_use]
    pub const fn asks(&self) -> u32 {
        self.asks
    }

    fn record(&mut self, outcome: AskOutcome, latency_ticks: u64, now_ticks: u64) {
        self.asks = self.asks.saturating_add(1);
        if outcome != AskOutcome::Silent {
            self.answered = self.answered.saturating_add(1);
            self.latency_ticks_total = self.latency_ticks_total.saturating_add(latency_ticks);
        }
        if outcome == AskOutcome::Conclusive {
            self.conclusive = self.conclusive.saturating_add(1);
        }
        self.last_observed_ticks = now_ticks;
    }

    /// The score in `0.0..=1.0`, relaxed toward [`UNOBSERVED_QUALITY`] by elapsed ticks.
    #[must_use]
    pub fn quality(&self, now_ticks: u64) -> f64 {
        if self.is_unobserved() {
            return UNOBSERVED_QUALITY;
        }
        let asks = f64::from(self.asks);
        let conclusive_rate = f64::from(self.conclusive) / asks;
        let answer_rate = f64::from(self.answered) / asks;
        let speed = if self.answered == 0 {
            0.0
        } else {
            let scale = LATENCY_SCALE_TICKS as f64;
            let mean = self.latency_ticks_total as f64 / f64::from(self.answered);
            scale / (scale + mean)
        };
        let observed = 0.6 * conclusive_rate + 0.25 * answer_rate + 0.15 * speed;

        let elapsed = now_ticks.saturating_sub(self.last_observed_ticks) as f64;
        let weight = 0.5f64.powf(elapsed / QUALITY_HALF_LIFE_TICKS as f64);
        UNOBSERVED_QUALITY + weight * (observed - UNOBSERVED_QUALITY)
    }
}

/// A peer that a forwarded ask can be routed to.
///
/// The routing identity MUST be derived from the **verified session identity** — the mTLS SPKI hash
/// this node computed itself — and never from a value carried in a message. That is the rule
/// `dig-node`'s neighbourhood probe already holds, and it is what keeps a peer from choosing where it
/// lands in our tiebreaks.
pub trait RoutablePeer: Copy + PartialEq {
    /// A stable node-local key for this peer, for deterministic tie-breaking only.
    fn routing_key(&self) -> u64;
}

/// This node's observations of the peers it currently holds connections to.
///
/// Deliberately a vector rather than a hash map: the pool is small and bounded, and an unordered
/// container is exactly what produced the defect this module exists to remove.
#[derive(Debug, Clone, PartialEq)]
pub struct AskObservations<Peer> {
    entries: Vec<(Peer, PeerObservations)>,
    capacity: usize,
}

impl<Peer: RoutablePeer> Default for AskObservations<Peer> {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_OBSERVATION_CAPACITY)
    }
}

impl<Peer: RoutablePeer> AskObservations<Peer> {
    /// An empty log holding at most `capacity` peers (at least one).
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: Vec::new(),
            capacity: capacity.max(1),
        }
    }

    /// Fold in the outcome of an ask **this node forwarded and saw complete**.
    ///
    /// When the ceiling is reached, the least-recently-observed peer is evicted to make room; the
    /// pool is the real bound, and this keeps the state finite if a caller forgets [`Self::retain`].
    pub fn record(&mut self, peer: Peer, outcome: AskOutcome, latency_ticks: u64, now_ticks: u64) {
        if let Some(entry) = self.entries.iter_mut().find(|(held, _)| *held == peer) {
            entry.1.record(outcome, latency_ticks, now_ticks);
            return;
        }
        if self.entries.len() >= self.capacity {
            self.evict_least_recently_observed();
        }
        let mut observations = PeerObservations::unobserved();
        observations.record(outcome, latency_ticks, now_ticks);
        self.entries.push((peer, observations));
    }

    /// Drop every peer no longer in the connected pool. Pool membership is the liveness gate, so a
    /// cycled-away peer leaves the candidate set without any TTL (SPEC §6.1, NC-12).
    pub fn retain(&mut self, pool: &[Peer]) {
        self.entries
            .retain(|(peer, _)| pool.iter().any(|held| held == peer));
    }

    /// What this node has observed of `peer` — the unobserved state if it has observed nothing.
    #[must_use]
    pub fn of(&self, peer: &Peer) -> PeerObservations {
        self.entries
            .iter()
            .find(|(held, _)| held == peer)
            .map_or_else(PeerObservations::unobserved, |(_, seen)| *seen)
    }

    /// How many peers are tracked.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing has been observed yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn evict_least_recently_observed(&mut self) {
        if let Some(index) = self
            .entries
            .iter()
            .enumerate()
            .min_by_key(|(_, (_, seen))| seen.last_observed_ticks)
            .map(|(index, _)| index)
        {
            self.entries.remove(index);
        }
    }
}

/// Everything the routing decision needs beyond the candidates themselves.
#[derive(Debug, Clone, Copy)]
pub struct AskRouting<'a, Peer> {
    /// The node-local tiebreak seed. Build it with [`SelectionSeed::from_peer_id`].
    pub seed: SelectionSeed,
    /// This node's own observations.
    pub observations: &'a AskObservations<Peer>,
    /// The current monotonic tick, for recency decay.
    pub now_ticks: u64,
}

/// Order `candidates` best-observed first, breaking ties deterministically.
///
/// Total and deterministic: equal scores fall through to the seeded identity tiebreak, and equal
/// tiebreaks (a key collision) fall through to the key itself, so the result never depends on the
/// order the candidates arrived in.
#[must_use]
pub fn rank<Peer: RoutablePeer>(routing: &AskRouting<'_, Peer>, candidates: &[Peer]) -> Vec<Peer> {
    let mut ranked: Vec<(f64, u64, u64, Peer)> = candidates
        .iter()
        .map(|peer| {
            let quality = routing.observations.of(peer).quality(routing.now_ticks);
            (
                quality,
                tiebreak(routing.seed, peer.routing_key()),
                peer.routing_key(),
                *peer,
            )
        })
        .collect();
    ranked.sort_by(|left, right| {
        right
            .0
            .partial_cmp(&left.0)
            .unwrap_or(core::cmp::Ordering::Equal)
            .then(left.1.cmp(&right.1))
            .then(left.2.cmp(&right.2))
    });
    ranked.into_iter().map(|(_, _, _, peer)| peer).collect()
}

/// Choose at most `fan_out` peers to forward an ask to, reserving the last slot for a peer this node
/// has never observed.
///
/// **The reserved slot is the whole reason this does not become a slower version of the defect it
/// replaces.** A ranking with no exploration converges on whichever peers happened to answer first
/// and then re-asks only those, which is load concentration again with better manners. Reserving one
/// slot keeps a path open for every pool member to earn a score.
///
/// The reservation applies only when `fan_out >= 2`: at a fan-out of one, spending the single slot on
/// exploration would mean never using what this node has learned.
#[must_use]
pub fn select_fan_out<Peer: RoutablePeer>(
    routing: &AskRouting<'_, Peer>,
    candidates: &[Peer],
    fan_out: usize,
) -> Vec<Peer> {
    let ranked = rank(routing, candidates);
    let mut chosen: Vec<Peer> = ranked.iter().take(fan_out).copied().collect();
    if fan_out < 2 || chosen.len() < fan_out {
        return chosen;
    }
    if chosen
        .iter()
        .any(|peer| routing.observations.of(peer).is_unobserved())
    {
        return chosen;
    }
    if let Some(explorer) = ranked
        .iter()
        .skip(fan_out)
        .find(|peer| routing.observations.of(peer).is_unobserved())
    {
        chosen.pop();
        chosen.push(*explorer);
    }
    chosen
}

/// The per-peer tiebreak value: the node-local seed mixed with the peer's routing identity.
fn tiebreak(seed: SelectionSeed, routing_key: u64) -> u64 {
    crate::selection::mix64(seed.raw() ^ crate::selection::mix64(routing_key))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test peer whose routing key is its id, so a fixture can be read at a glance.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct TestPeer(u64);

    impl RoutablePeer for TestPeer {
        fn routing_key(&self) -> u64 {
            self.0
        }
    }

    fn seed() -> SelectionSeed {
        SelectionSeed::from_peer_id(&[7u8; 32])
    }

    fn routing<'a>(
        observations: &'a AskObservations<TestPeer>,
        now: u64,
    ) -> AskRouting<'a, TestPeer> {
        AskRouting {
            seed: seed(),
            observations,
            now_ticks: now,
        }
    }

    fn pool(ids: &[u64]) -> Vec<TestPeer> {
        ids.iter().copied().map(TestPeer).collect()
    }

    /// A conclusive, fast, recent answer must outrank an unobserved peer, which must in turn outrank
    /// a peer observed only to be silent. The fixture carries all three classes at once, because a
    /// two-class fixture cannot see a comparator that collapses one boundary.
    #[test]
    fn a_good_answerer_outranks_an_unobserved_peer_which_outranks_a_silent_one() {
        let mut observations = AskObservations::with_capacity(16);
        observations.record(TestPeer(1), AskOutcome::Conclusive, 1, 100);
        observations.record(TestPeer(3), AskOutcome::Silent, 0, 100);

        let ranked = rank(&routing(&observations, 100), &pool(&[3, 2, 1]));

        assert_eq!(ranked, pool(&[1, 2, 3]));
    }

    /// An honest absence must rank above silence, so that answering is never worse than not
    /// answering (SPEC §8.2A).
    #[test]
    fn an_inconclusive_answer_ranks_above_silence() {
        let mut observations = AskObservations::with_capacity(16);
        observations.record(TestPeer(1), AskOutcome::Inconclusive, 1, 100);
        observations.record(TestPeer(2), AskOutcome::Silent, 0, 100);

        let inconclusive = observations.of(&TestPeer(1)).quality(100);
        let silent = observations.of(&TestPeer(2)).quality(100);

        assert!(
            inconclusive > silent,
            "answering ({inconclusive}) must beat silence ({silent})"
        );
    }

    /// Both praise and blame relax toward the unobserved baseline, so an old verdict can never become
    /// a permanent exclusion. Asserting BOTH directions matters: a decay toward zero would still pass
    /// a test that only watched a good peer fade.
    #[test]
    fn stale_praise_and_stale_blame_both_decay_toward_the_unobserved_baseline() {
        let mut observations = AskObservations::with_capacity(16);
        observations.record(TestPeer(1), AskOutcome::Conclusive, 0, 0);
        observations.record(TestPeer(2), AskOutcome::Silent, 0, 0);

        let far_future = QUALITY_HALF_LIFE_TICKS * 12;
        let faded_good = observations.of(&TestPeer(1)).quality(far_future);
        let faded_bad = observations.of(&TestPeer(2)).quality(far_future);

        assert!(observations.of(&TestPeer(1)).quality(0) > UNOBSERVED_QUALITY);
        assert!(observations.of(&TestPeer(2)).quality(0) < UNOBSERVED_QUALITY);
        assert!(
            (faded_good - UNOBSERVED_QUALITY).abs() < 1e-3,
            "{faded_good}"
        );
        assert!((faded_bad - UNOBSERVED_QUALITY).abs() < 1e-3, "{faded_bad}");
    }

    /// The tiebreak must depend on the peer, not on where the peer appeared in the input — input
    /// order IS the `HashMap` order this module exists to stop honouring. The fixture feeds the same
    /// unobserved pool in three different orders, including reversed and rotated, because a
    /// comparator that merely preserved the input order passes a single-permutation test.
    #[test]
    fn ties_break_on_identity_so_input_order_cannot_reach_the_result() {
        let observations = AskObservations::<TestPeer>::with_capacity(16);
        let route = routing(&observations, 0);

        let baseline = rank(&route, &pool(&[1, 2, 3, 4, 5, 6, 7, 8]));
        for order in [
            pool(&[8, 7, 6, 5, 4, 3, 2, 1]),
            pool(&[5, 6, 7, 8, 1, 2, 3, 4]),
            pool(&[3, 1, 4, 8, 6, 2, 7, 5]),
        ] {
            assert_eq!(rank(&route, &order), baseline);
        }
    }

    /// A tiebreak that ignored the seed would give every node in the network the same order, which
    /// re-concentrates load globally instead of per-process. Different nodes must disagree.
    #[test]
    fn the_tiebreak_order_differs_between_nodes() {
        let observations = AskObservations::<TestPeer>::with_capacity(16);
        let candidates = pool(&[1, 2, 3, 4, 5, 6, 7, 8]);

        let ours = rank(
            &AskRouting {
                seed: SelectionSeed::from_peer_id(&[7u8; 32]),
                observations: &observations,
                now_ticks: 0,
            },
            &candidates,
        );
        let theirs = rank(
            &AskRouting {
                seed: SelectionSeed::from_peer_id(&[9u8; 32]),
                observations: &observations,
                now_ticks: 0,
            },
            &candidates,
        );

        assert_ne!(ours, theirs);
    }

    /// The exploration slot: once the top of the ranking is entirely observed, the last slot goes to
    /// an unobserved peer. Without it the ranking self-locks onto its first winners and the
    /// concentration defect returns in a subtler form.
    #[test]
    fn an_unobserved_peer_always_holds_a_slot_in_the_slate() {
        let mut observations = AskObservations::with_capacity(16);
        for id in 1..=4 {
            observations.record(TestPeer(id), AskOutcome::Conclusive, 1, 100);
        }
        let candidates = pool(&[1, 2, 3, 4, 5, 6]);

        let slate = select_fan_out(&routing(&observations, 100), &candidates, 3);

        assert_eq!(slate.len(), 3);
        assert!(
            slate
                .iter()
                .any(|peer| observations.of(peer).is_unobserved()),
            "slate {slate:?} reserved no slot for an unobserved peer"
        );
        assert!(
            slate
                .iter()
                .filter(|peer| !observations.of(peer).is_unobserved())
                .count()
                == 2,
            "only ONE slot is reserved; the rest still go to the best observed peers"
        );
    }

    /// Repeated asks must not always land on the same unobserved peer either: as each explorer earns
    /// a score it stops being the explorer, so the whole pool is reachable. This is the property the
    /// acceptance measurement calls "spread".
    #[test]
    fn exploration_reaches_every_peer_in_the_pool() {
        let mut observations = AskObservations::with_capacity(32);
        let candidates = pool(&[1, 2, 3, 4, 5, 6, 7, 8, 9]);

        for round in 0..12u64 {
            let slate = select_fan_out(&routing(&observations, round * 10), &candidates, 3);
            for peer in slate {
                observations.record(peer, AskOutcome::Inconclusive, 1, round * 10);
            }
        }

        for peer in &candidates {
            assert!(
                !observations.of(peer).is_unobserved(),
                "{peer:?} was never asked"
            );
        }
    }

    /// With a fan-out of one there is no slot to spare: the single ask goes to the best peer, because
    /// spending it on exploration would mean never using what this node has learned.
    #[test]
    fn a_fan_out_of_one_is_not_spent_on_exploration() {
        let mut observations = AskObservations::with_capacity(16);
        observations.record(TestPeer(1), AskOutcome::Conclusive, 1, 100);

        let slate = select_fan_out(&routing(&observations, 100), &pool(&[1, 2, 3]), 1);

        assert_eq!(slate, pool(&[1]));
    }

    /// SPEC §8.4: the state is bounded even if a caller never calls `retain`. The fixture pushes far
    /// past the ceiling and asserts BOTH that the size holds and that the most recently observed peer
    /// survived — an eviction policy that dropped the newest would also hold the size.
    #[test]
    fn the_observation_log_is_bounded_and_evicts_the_least_recently_observed() {
        let mut observations = AskObservations::with_capacity(4);
        for id in 1..=40u64 {
            observations.record(TestPeer(id), AskOutcome::Conclusive, 1, id);
        }

        assert_eq!(observations.len(), 4);
        assert!(!observations.of(&TestPeer(40)).is_unobserved());
        assert!(observations.of(&TestPeer(1)).is_unobserved());
    }

    /// Pool membership is the liveness gate: a peer that cycled out of the pool leaves the candidate
    /// set immediately, with no TTL to get wrong (NC-12).
    #[test]
    fn observations_are_dropped_when_the_peer_leaves_the_pool() {
        let mut observations = AskObservations::with_capacity(16);
        observations.record(TestPeer(1), AskOutcome::Conclusive, 1, 10);
        observations.record(TestPeer(2), AskOutcome::Conclusive, 1, 10);

        observations.retain(&pool(&[2]));

        assert!(observations.of(&TestPeer(1)).is_unobserved());
        assert!(!observations.of(&TestPeer(2)).is_unobserved());
        assert_eq!(observations.len(), 1);
    }
}
