//! Recursive discovery on an inbound miss (SPEC §6).
//!
//! A node asked for a store it does not hold can ask **its own peers** rather than answering a bare
//! absence. Recursion is what makes a partial peer set usable: every node's peer slice is a narrow,
//! arbitrary sample of the network, and an answer this node cannot give may be one hop away. A global
//! provider index cannot substitute — it cannot reach a holder it has never heard of, nor one
//! reachable only through a peer's peer — so the two are complementary.
//!
//! # The cost is an EXPONENT, and that is why the defaults are hostile
//!
//! Reach is `fan_out ^ hop_cap`, so the defaults here are deliberately small: one admitted request
//! can otherwise recruit hundreds of nodes.
//! [`RecursionConfig::worst_case_nodes_recruited`] states that number rather than leaving it to be
//! discovered in production, and **a concurrency ceiling is not a substitute**: it bounds how many
//! hops happen at once, never how many happen.
//!
//! # OFF by default, and unrecognised configuration FAILS CLOSED
//!
//! This path spends **other** nodes' bandwidth, so it must not be gated more loosely than one spending
//! only this node's. [`RecursionConfig::default`] is disabled, and [`parse_enabled`] treats anything
//! it does not recognise as disabled — a typo must never be able to enable a network-wide amplifier.
//!
//! A disabled node forwards **nothing**, including relaying for others. The switch cuts the chain at
//! every disabled node, not only at originators.
//!
//! # Whom the ask goes to is a DECISION, not the order the pool arrived in
//!
//! The `fan_out` peers are chosen by [`crate::routing`], which ranks them by what this node has
//! observed of their answers and reserves one slot for a peer it has never observed. Taking a prefix
//! of the caller's slice instead would concentrate every forwarded ask on a fixed arbitrary handful,
//! because the pool reaches this crate from a `HashMap`.
//!
//! # Answers from a hop are HEARSAY
//!
//! They are candidates for the **fetch** path, where verification makes a wrong candidate merely
//! wasted work. They are marked as such by [`Provenance`] and MUST NOT be asserted to a third party
//! as this node's own knowledge — nor allowed to displace a locally-known holder, which is why the
//! answer cap falls on the forwarded portion only.
//!
//! # Disclosure
//!
//! The disclosure radius is [`RecursionConfig::worst_case_nodes_recruited`] nodes, none of them chosen
//! or enumerable by the requestor. The absence of a requestor identity is **not anonymity** — a peer
//! is free to log what it was asked — and the disclosure happens on a MISS, precisely when the
//! requestor has not yet decided to contact any holder. It is therefore not a disclosure a completed
//! direct read would have made anyway.

use crate::routing::{select_fan_out, AskRouting, RoutablePeer};

/// Where an answer came from, and therefore how much this node may assert about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// This node knows it first-hand. It may be asserted and may never be displaced by hearsay.
    FirstHand,
    /// Relayed by a hop. A **candidate for the fetch path only**, never asserted to a third party.
    Hearsay,
}

/// The recursion switch and its bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecursionConfig {
    /// Whether this node participates at all — as an originator OR as a relay.
    pub enabled: bool,
    /// How many peers one miss may be forwarded to.
    pub fan_out: u8,
    /// How many hops a request may travel.
    pub hop_cap: u8,
    /// The maximum number of HEARSAY answers admitted into a response. First-hand answers are not
    /// subject to it (SPEC §6.1.6).
    pub max_hearsay_answers: usize,
}

impl Default for RecursionConfig {
    /// **Disabled**, with deliberately small bounds for when it is switched on.
    fn default() -> Self {
        Self {
            enabled: false,
            fan_out: 3,
            hop_cap: 2,
            max_hearsay_answers: 8,
        }
    }
}

impl RecursionConfig {
    /// The number of nodes one admitted request can recruit in the worst case: `fan_out ^ hop_cap`.
    ///
    /// Stated as a function so the real cost is a value an operator can read, not an exponent they
    /// have to work out. This is also the **disclosure radius** (SPEC §6.2).
    #[must_use]
    pub fn worst_case_nodes_recruited(&self) -> u64 {
        u64::from(self.fan_out).saturating_pow(u32::from(self.hop_cap))
    }
}

/// Parse the recursion switch, **failing closed**.
///
/// Only explicitly-affirmative values enable it. Anything unrecognised — a typo, an empty string, a
/// value from a newer config format — disables it, because the failure this protects against is a
/// mistake silently enabling a network-wide amplifier.
#[must_use]
pub fn parse_enabled(raw: Option<&str>) -> bool {
    matches!(
        raw.map(str::trim).map(str::to_ascii_lowercase).as_deref(),
        Some("1" | "on" | "true" | "yes")
    )
}

/// A request to forward, as this node received it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InboundAsk<Peer> {
    /// Who asked. Excluded from the fan-out, and never asked its own question back.
    pub requestor: Peer,
    /// The hops remaining, as carried IN the request. `None` means the budget could not be read.
    pub hops_remaining: Option<u8>,
}

/// Why a miss will not be forwarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForwardRefusal {
    /// Recursion is switched off on this node. A disabled node relays nothing.
    Disabled,
    /// The hop budget is exhausted.
    HopBudgetSpent,
    /// The hop budget could not be read from the request. **Refused, never forwarded
    /// optimistically** — a request whose budget is unreadable is a request whose reach is unbounded.
    UnreadableHopBudget,
    /// No eligible peer remained after excluding the requestor and this node.
    NoEligiblePeers,
    /// The relay budget for work done on others' behalf is exhausted (SPEC §6.1.8).
    RelayBudgetSpent,
}

/// The decision about an inbound miss.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForwardDecision<Peer> {
    /// Ask these peers, with `hops_remaining` decremented.
    Forward {
        /// The peers to ask — never the requestor, never this node, never longer than `fan_out`.
        peers: Vec<Peer>,
        /// The budget to carry in the forwarded request.
        hops_remaining: u8,
    },
    /// Do not forward, for this reason.
    Refuse(ForwardRefusal),
}

/// Decide whether and to whom to forward an inbound miss.
///
/// `relay_budget_available` is the caller's separate allowance for work done on OTHERS' behalf. It is
/// distinct from this node's own request budget on purpose: billing a hop's fan-out to the hop's own
/// allowance lets one admitted request spend a victim's budget across every peer it holds.
///
/// `routing` supplies the order. **The peers are chosen by
/// [`select_fan_out`](crate::routing::select_fan_out), never by taking a prefix of `known_peers`**:
/// the pool arrives from a `HashMap`, so a prefix is a fixed arbitrary sample and the same few peers
/// would absorb every forwarded ask for the life of the process (SPEC §6.1.2).
#[must_use]
pub fn decide_forward<Peer: RoutablePeer>(
    config: &RecursionConfig,
    ask: &InboundAsk<Peer>,
    this_node: &Peer,
    known_peers: &[Peer],
    relay_budget_available: bool,
    routing: &AskRouting<'_, Peer>,
) -> ForwardDecision<Peer> {
    if !config.enabled {
        return ForwardDecision::Refuse(ForwardRefusal::Disabled);
    }
    let Some(hops_remaining) = ask.hops_remaining else {
        return ForwardDecision::Refuse(ForwardRefusal::UnreadableHopBudget);
    };
    if hops_remaining == 0 {
        return ForwardDecision::Refuse(ForwardRefusal::HopBudgetSpent);
    }
    if !relay_budget_available {
        return ForwardDecision::Refuse(ForwardRefusal::RelayBudgetSpent);
    }

    let eligible: Vec<Peer> = known_peers
        .iter()
        .filter(|peer| **peer != ask.requestor && **peer != *this_node)
        .copied()
        .collect();
    let peers = select_fan_out(routing, &eligible, config.fan_out as usize);

    if peers.is_empty() {
        return ForwardDecision::Refuse(ForwardRefusal::NoEligiblePeers);
    }
    ForwardDecision::Forward {
        peers,
        hops_remaining: hops_remaining - 1,
    }
}

/// Merge first-hand and relayed answers into one response.
///
/// **The cap falls on the HEARSAY portion only.** Capping the merged set instead would let one peer
/// returning a full slate of fabricated holders evict every genuine holder from the answer, for free —
/// which is a denial of the answer itself, achieved without holding anything.
#[must_use]
pub fn merge_answers<Answer: Copy>(
    config: &RecursionConfig,
    first_hand: &[Answer],
    hearsay: &[Answer],
) -> Vec<(Answer, Provenance)> {
    let mut merged: Vec<(Answer, Provenance)> = first_hand
        .iter()
        .map(|answer| (*answer, Provenance::FirstHand))
        .collect();
    merged.extend(
        hearsay
            .iter()
            .take(config.max_hearsay_answers)
            .map(|answer| (*answer, Provenance::Hearsay)),
    );
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::{AskObservations, AskOutcome};
    use crate::selection::SelectionSeed;

    /// A test peer whose routing key is its id, so a fixture can be read at a glance.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct P(u64);

    impl RoutablePeer for P {
        fn routing_key(&self) -> u64 {
            self.0
        }
    }

    fn pool(ids: &[u64]) -> Vec<P> {
        ids.iter().copied().map(P).collect()
    }

    fn unobserved() -> AskObservations<P> {
        AskObservations::with_capacity(64)
    }

    fn routing<'a>(observations: &'a AskObservations<P>) -> AskRouting<'a, P> {
        AskRouting {
            seed: SelectionSeed::from_peer_id(&[7u8; 32]),
            observations,
            now_ticks: 100,
        }
    }

    fn enabled() -> RecursionConfig {
        RecursionConfig {
            enabled: true,
            ..RecursionConfig::default()
        }
    }

    fn ask(requestor: u64, hops_remaining: Option<u8>) -> InboundAsk<P> {
        InboundAsk {
            requestor: P(requestor),
            hops_remaining,
        }
    }

    /// SPEC §6.1.3: OFF by default, and unrecognised values fail closed. A typo must never be able to
    /// enable a network-wide amplifier, so the negative cases include a near-miss spelling rather than
    /// only obviously-false values.
    #[test]
    fn recursion_is_off_by_default_and_unrecognised_configuration_fails_closed() {
        assert!(!RecursionConfig::default().enabled);
        assert!(parse_enabled(Some("on")));
        assert!(parse_enabled(Some("TRUE")));
        for raw in [None, Some(""), Some("yse"), Some("enabled"), Some("off")] {
            assert!(!parse_enabled(raw), "{raw:?} must not enable recursion");
        }
    }

    /// SPEC §6.1.4: a disabled node forwards NOTHING, including relaying for others. The fixture is a
    /// relay case — a healthy budget and hops remaining — so an implementation that only checked the
    /// switch for originators would forward here.
    #[test]
    fn a_disabled_node_relays_nothing_for_others() {
        let decision = decide_forward(
            &RecursionConfig::default(),
            &ask(1, Some(2)),
            &P(0),
            &pool(&[2, 3, 4]),
            true,
            &routing(&unobserved()),
        );
        assert_eq!(decision, ForwardDecision::Refuse(ForwardRefusal::Disabled));
    }

    /// SPEC §6.1.1: a hop that cannot read the budget REFUSES rather than forwarding optimistically.
    /// An unreadable budget is an unbounded one, so the optimistic choice is the unbounded choice.
    #[test]
    fn an_unreadable_hop_budget_refuses_rather_than_forwarding() {
        assert_eq!(
            decide_forward(
                &enabled(),
                &ask(1, None),
                &P(0),
                &pool(&[2, 3, 4]),
                true,
                &routing(&unobserved())
            ),
            ForwardDecision::Refuse(ForwardRefusal::UnreadableHopBudget)
        );
    }

    /// The budget is honoured on RECEIPT, not only on send: a request arriving with zero hops left is
    /// not forwarded, and a forwarded request always carries a decremented budget.
    #[test]
    fn the_hop_budget_is_honoured_on_receipt_and_decremented_on_send() {
        assert_eq!(
            decide_forward(
                &enabled(),
                &ask(1, Some(0)),
                &P(0),
                &pool(&[2, 3]),
                true,
                &routing(&unobserved())
            ),
            ForwardDecision::Refuse(ForwardRefusal::HopBudgetSpent)
        );

        let ForwardDecision::Forward { hops_remaining, .. } = decide_forward(
            &enabled(),
            &ask(1, Some(2)),
            &P(0),
            &pool(&[2, 3]),
            true,
            &routing(&unobserved()),
        ) else {
            panic!("expected a forward");
        };
        assert_eq!(hops_remaining, 1);
    }

    /// SPEC §6.1.7: the requestor and this node are excluded, so a hop is never asked its own question
    /// back. The fixture puts BOTH in the peer list — a filter that removed only one still passes a
    /// single-exclusion test.
    #[test]
    fn the_requestor_and_this_node_are_excluded_from_the_fan_out() {
        let ForwardDecision::Forward { peers, .. } = decide_forward(
            &enabled(),
            &ask(1, Some(2)),
            &P(0),
            &pool(&[0, 1, 2, 3]),
            true,
            &routing(&unobserved()),
        ) else {
            panic!("expected a forward");
        };
        assert_eq!(peers.len(), 2, "{peers:?}");
        assert!(
            !peers.contains(&P(0)) && !peers.contains(&P(1)),
            "{peers:?}"
        );
    }

    /// SPEC §6.1.2: the fan-out bounds how many peers one miss recruits per hop.
    #[test]
    fn the_fan_out_bounds_the_peers_asked() {
        let config = RecursionConfig {
            fan_out: 2,
            ..enabled()
        };
        let ForwardDecision::Forward { peers, .. } = decide_forward(
            &config,
            &ask(9, Some(2)),
            &P(0),
            &pool(&[1, 2, 3, 4, 5]),
            true,
            &routing(&unobserved()),
        ) else {
            panic!("expected a forward");
        };
        assert_eq!(peers.len(), 2);
    }

    /// **The defect this ranking replaced.** The pool reaches `decide_forward` from a `HashMap`, so
    /// the slice order is arbitrary but STABLE for a given map instance — a prefix is therefore a
    /// fixed arbitrary sample, and the same three peers absorb every forwarded ask for the life of
    /// the process while the rest of the pool is never asked at all.
    ///
    /// The fixture feeds the SAME pool in several orders, which is what a differently-seeded
    /// `HashMap` produces, and requires the slate to be identical every time. A prefix cannot satisfy
    /// that: it returns whatever the arbitrary order handed it. A control asserts the pool really is
    /// large enough for the choice to matter, so the test cannot pass vacuously on a pool of three.
    #[test]
    fn the_slate_is_ranked_not_the_arbitrary_prefix_of_the_pool() {
        let observations = unobserved();
        let orders = [
            pool(&[2, 3, 4, 5, 6, 7, 8, 9]),
            pool(&[9, 8, 7, 6, 5, 4, 3, 2]),
            pool(&[5, 9, 2, 7, 4, 8, 3, 6]),
        ];
        assert!(orders[0].len() > usize::from(enabled().fan_out) * 2);

        let slates: Vec<Vec<P>> = orders
            .iter()
            .map(|order| {
                let ForwardDecision::Forward { peers, .. } = decide_forward(
                    &enabled(),
                    &ask(1, Some(2)),
                    &P(0),
                    order,
                    true,
                    &routing(&observations),
                ) else {
                    panic!("expected a forward");
                };
                peers
            })
            .collect();

        assert_eq!(slates[0], slates[1], "the slate followed the input order");
        assert_eq!(slates[0], slates[2], "the slate followed the input order");
        assert_ne!(
            slates[0],
            orders[0][..3].to_vec(),
            "the slate is still the arbitrary prefix"
        );
    }

    /// A peer this node has observed answering conclusively must reach the slate even when the
    /// arbitrary pool order buries it at the end — which is the routing-quality half of the defect.
    /// One good peer among many unobserved ones is the fixture that distinguishes "ranked" from
    /// "shuffled": a shuffle would place it in the slate only by chance.
    #[test]
    fn an_observed_good_answerer_reaches_the_slate_from_the_back_of_the_pool() {
        let mut observations = unobserved();
        observations.record(P(9), AskOutcome::Conclusive, 1, 100);

        let ForwardDecision::Forward { peers, .. } = decide_forward(
            &enabled(),
            &ask(1, Some(2)),
            &P(0),
            &pool(&[2, 3, 4, 5, 6, 7, 8, 9]),
            true,
            &routing(&observations),
        ) else {
            panic!("expected a forward");
        };

        assert!(peers.contains(&P(9)), "{peers:?}");
        assert!(
            peers
                .iter()
                .any(|peer| observations.of(peer).is_unobserved()),
            "the exploration slot must survive into decide_forward: {peers:?}"
        );
    }

    /// SPEC §6.1.2/§6.2: the real per-request cost is an exponent and is stated, not left implicit.
    #[test]
    fn the_worst_case_recruitment_is_the_exponent_not_the_fan_out() {
        let config = RecursionConfig {
            fan_out: 3,
            hop_cap: 4,
            ..enabled()
        };
        assert_eq!(
            config.worst_case_nodes_recruited(),
            81,
            "reach is fan_out ^ hop_cap, not fan_out"
        );
    }

    /// SPEC §6.1.8: relayed work draws on a SEPARATE budget. With the relay allowance exhausted the
    /// forward is refused even though everything else — switch, hops, peers — permits it.
    #[test]
    fn relayed_work_is_refused_when_the_separate_relay_budget_is_spent() {
        assert_eq!(
            decide_forward(
                &enabled(),
                &ask(1, Some(2)),
                &P(0),
                &pool(&[2, 3]),
                false,
                &routing(&unobserved())
            ),
            ForwardDecision::Refuse(ForwardRefusal::RelayBudgetSpent)
        );
    }

    /// SPEC §6.1.5/§6.1.6 — the load-bearing merge property. A peer floods a full slate of fabricated
    /// holders; every first-hand answer must survive, and the flood must be capped. An implementation
    /// that capped the MERGED set drops genuine holders here, which is a denial of the answer achieved
    /// without holding anything.
    #[test]
    fn a_flood_of_hearsay_cannot_evict_a_locally_known_holder() {
        let config = RecursionConfig {
            max_hearsay_answers: 2,
            ..enabled()
        };
        let fabricated: Vec<u8> = (100..200).collect();

        let merged = merge_answers(&config, &[1, 2, 3], &fabricated);

        let surviving_first_hand: Vec<u8> = merged
            .iter()
            .filter(|(_, provenance)| *provenance == Provenance::FirstHand)
            .map(|(answer, _)| *answer)
            .collect();
        assert_eq!(
            surviving_first_hand,
            vec![1, 2, 3],
            "every genuine holder must survive a hearsay flood"
        );
        assert_eq!(
            merged.len(),
            5,
            "the cap falls on the forwarded portion only"
        );
    }

    /// Relayed answers are marked HEARSAY so a caller cannot assert them to a third party as its own
    /// knowledge. Unmarked, the two are indistinguishable at the point of use.
    #[test]
    fn relayed_answers_are_marked_as_hearsay() {
        let merged = merge_answers(&enabled(), &[1], &[2]);
        assert_eq!(merged[0], (1, Provenance::FirstHand));
        assert_eq!(merged[1], (2, Provenance::Hearsay));
    }
}
