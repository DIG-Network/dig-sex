//! Peer conduct — separating a malicious peer from a distressed honest one (SPEC §8.2A).
//!
//! A peer that claims to hold content and then does not deliver it is evidence. **Evidence of *what*
//! depends entirely on HOW it failed**, and collapsing the cases is the defect this module exists to
//! prevent: a liar and a peer that is overloaded, rate-limited, half-partitioned or mid-restart
//! produce the SAME observable at the transport layer — nothing arrived.
//!
//! # Local evidence, not on-chain evidence
//!
//! [`ConductEvidence`] is this crate's own peer-scoped type. It records **what a peer did to this
//! node** and carries no on-chain authority whatsoever. It deliberately does not reuse an L2 evidence
//! type: borrowing that vocabulary would dress a local observation as a chain-anchored fact.
//!
//! # Why non-performance may never carry a durable penalty
//!
//! An adversary who can degrade an honest peer — by load, by occupying its connection slots, by
//! partitioning it — can make that peer look unreliable **to everyone else**. So a non-performance
//! penalty is not merely unreliable, it is *weaponisable against a peer the attacker does not
//! control*. Three properties follow, and all three are enforced here rather than documented:
//!
//! 1. non-performance penalties **decay** on the same monotonic tick basis as everything else, so a
//!    distressed peer recovers without intervening and without proving anything;
//! 2. a non-performance penalty **never reaches zero dial share** — a peer that can never be retried
//!    can never demonstrate recovery, which turns a transient penalty into a permanent exclusion;
//! 3. the penalty is **cheap enough that inducing it is not worth an attacker's effort**. If degrading
//!    a competitor costs less than serving content, the reputation system has become the attack.
//!
//! # Reputation is LOCAL and is never gossiped as fact
//!
//! There is deliberately no way to import another peer's assessment of a third party, and no way to
//! export this node's. Gossiped reputation is a defamation primitive: it degrades a peer everywhere at
//! once without the attacker ever interacting with it, and the recipient cannot verify it. **A peer's
//! misconduct is demonstrated to this node by that peer, or it is not demonstrated.**
//!
//! # Purity
//!
//! Nothing here reads a clock or a file. Persisted conduct enters as a caller-supplied
//! [`ConductRecord`] and time enters as a tick, exactly as SPEC §1.3 and §2A.5 require.

/// What this node observed a peer do. The classes are kept distinct because only two of them are
/// verifiable, and conflating them is what makes an honest holder brandable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConductEvidence {
    /// Delivered bytes that fail verification against the chain-anchored root. **Verifiable** —
    /// arithmetic, not judgement.
    ProvenLie,
    /// Claimed to hold content, then answered a direct request for it with an absence. **Verifiable**
    /// — the peer contradicted its own claim.
    SelfContradiction,
    /// Timeout, reset, silence, truncation, or persistent slowness. **NOT verifiable** —
    /// indistinguishable from distress, and manufacturable in a third party.
    NonPerformance,
    /// Answered honestly, including an honest "I do not have it".
    ///
    /// Present so that answering is never worse than silence: an honest absence is a RESPONSE, and
    /// SPEC §8.2A requires that declining to respond must not rank a peer better than answering.
    HonestAnswer,
}

impl ConductEvidence {
    /// Whether this class may carry a **durable** penalty. Only the two verifiable classes may; see
    /// the module docs for why non-performance may not.
    #[must_use]
    pub const fn is_verifiable(self) -> bool {
        matches!(
            self,
            ConductEvidence::ProvenLie | ConductEvidence::SelfContradiction
        )
    }
}

/// The per-peer conduct state this node has accumulated. Supplied by the caller (it is I/O when it
/// persists) and returned updated — the core never reads or writes storage itself.
///
/// An unreadable persisted record MUST be replaced with [`ConductRecord::neutral`], never with a
/// penalised one: losing a file must not become an exclusion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ConductRecord {
    /// Count of verifiable misconduct. Durable — it never decays, because it is a fact about what the
    /// peer said and did, checkable without trusting anyone.
    pub proven_faults: u32,
    /// Transient non-performance penalty, in penalty units. Decays toward zero over time.
    pub non_performance: u32,
    /// The tick at which `non_performance` was last updated, so decay is computed from elapsed ticks
    /// rather than from a clock.
    pub last_update_ticks: u64,
}

impl ConductRecord {
    /// The state a peer starts in and the state an unreadable record falls back to.
    #[must_use]
    pub fn neutral() -> Self {
        Self::default()
    }
}

/// Penalty units added per observed non-performance. Deliberately small relative to
/// [`NON_PERFORMANCE_CEILING`]: inducing one failure barely moves a peer's standing, so an attacker
/// must sustain the degradation for as long as they want the effect — which is the "cheap enough that
/// inducing it is not worth the effort" property, expressed as a number.
pub const NON_PERFORMANCE_PENALTY: u32 = 1;

/// The ceiling on accumulated non-performance. Bounds the state (SPEC §8.4) and bounds the WORST
/// outcome an attacker can drive a third party to — which, by [`dial_share`], is still non-zero.
pub const NON_PERFORMANCE_CEILING: u32 = 8;

/// Ticks after which one penalty unit decays. Recovery is automatic and unconditional: a distressed
/// peer needs neither to intervene nor to prove anything.
pub const NON_PERFORMANCE_DECAY_TICKS: u64 = 600;

/// The dial share floor a non-performing peer can never fall below.
///
/// **Non-zero on purpose.** A peer that is never retried can never demonstrate recovery, so a zero
/// floor would silently convert this transient penalty into the permanent exclusion SPEC §8.2A exists
/// to prevent.
pub const MIN_NON_PERFORMANCE_DIAL_SHARE: f64 = 0.1;

/// Apply an observation to a peer's record at `now_ticks`.
///
/// Decay is applied FIRST, so a penalty already earned back is not re-punished by an observation that
/// arrives long afterwards.
#[must_use]
pub fn observe(record: ConductRecord, evidence: ConductEvidence, now_ticks: u64) -> ConductRecord {
    let mut updated = decay(record, now_ticks);
    match evidence {
        ConductEvidence::ProvenLie | ConductEvidence::SelfContradiction => {
            updated.proven_faults = updated.proven_faults.saturating_add(1);
        }
        ConductEvidence::NonPerformance => {
            updated.non_performance = updated
                .non_performance
                .saturating_add(NON_PERFORMANCE_PENALTY)
                .min(NON_PERFORMANCE_CEILING);
        }
        // An honest answer earns no penalty and, deliberately, no reward: a peer must not be able to
        // raise its own standing by talking (SPEC §8.1 — demote on evidence, never promote on a
        // declaration). It ranks above silence because silence accrues non-performance and this does
        // not.
        ConductEvidence::HonestAnswer => {}
    }
    updated
}

/// Decay the transient penalty to `now_ticks`. Durable faults are untouched.
#[must_use]
pub fn decay(record: ConductRecord, now_ticks: u64) -> ConductRecord {
    let elapsed = now_ticks.saturating_sub(record.last_update_ticks);
    let decayed = u32::try_from(elapsed / NON_PERFORMANCE_DECAY_TICKS).unwrap_or(u32::MAX);
    ConductRecord {
        proven_faults: record.proven_faults,
        non_performance: record.non_performance.saturating_sub(decayed),
        last_update_ticks: now_ticks,
    }
}

/// The share of dial attempts this peer should receive, in `[0, 1]`.
///
/// A **proven** fault may reduce the share to zero: that is a verified fact about the peer, and SPEC
/// §8.3 permits a durable exclusion earned by a proven lie. Non-performance alone never can — it is
/// floored at [`MIN_NON_PERFORMANCE_DIAL_SHARE`] so recovery stays demonstrable.
///
/// This function only ever DEMOTES. There is no input by which a peer can raise its own share, which
/// is SPEC §8.1's asymmetry made structural rather than documented.
#[must_use]
pub fn dial_share(record: ConductRecord) -> f64 {
    if record.proven_faults > 0 {
        return 0.0;
    }
    let penalty = f64::from(record.non_performance) / f64::from(NON_PERFORMANCE_CEILING);
    (1.0 - penalty).max(MIN_NON_PERFORMANCE_DIAL_SHARE)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPEC §8.2A: the two verifiable classes are durable; non-performance is not. Asserted on the
    /// classifier AND on the resulting state, because a correct classifier wired to a handler that
    /// treats every class alike still produces the defect.
    #[test]
    fn only_verifiable_misconduct_becomes_a_durable_fault() {
        assert!(ConductEvidence::ProvenLie.is_verifiable());
        assert!(ConductEvidence::SelfContradiction.is_verifiable());
        assert!(!ConductEvidence::NonPerformance.is_verifiable());

        let lied = observe(ConductRecord::neutral(), ConductEvidence::ProvenLie, 0);
        let stalled = observe(ConductRecord::neutral(), ConductEvidence::NonPerformance, 0);

        assert_eq!(lied.proven_faults, 1);
        assert_eq!(lied.non_performance, 0);
        assert_eq!(stalled.proven_faults, 0, "a timeout is not a proven fault");
        assert_eq!(stalled.non_performance, NON_PERFORMANCE_PENALTY);
    }

    /// SPEC §8.2A.1: a distressed peer recovers without intervening. The fixture drives the peer to
    /// the WORST reachable state and then only advances time — no honest answer, no proof, nothing
    /// the peer had to do — and the share must return to full.
    #[test]
    fn a_distressed_peer_recovers_on_time_alone() {
        let mut record = ConductRecord::neutral();
        for _ in 0..NON_PERFORMANCE_CEILING * 2 {
            record = observe(record, ConductEvidence::NonPerformance, 0);
        }
        assert_eq!(record.non_performance, NON_PERFORMANCE_CEILING);

        let recovered = decay(
            record,
            NON_PERFORMANCE_DECAY_TICKS * u64::from(NON_PERFORMANCE_CEILING),
        );
        assert_eq!(recovered.non_performance, 0);
        assert_eq!(dial_share(recovered), 1.0);
    }

    /// SPEC §8.2A.2 — the load-bearing bound. The fixture saturates the penalty far beyond its
    /// ceiling, i.e. the most an attacker can achieve by manufacturing distress in a third party, and
    /// the share must still be strictly positive. A peer at zero share can never be retried, so it can
    /// never demonstrate recovery, and the transient penalty has become permanent.
    #[test]
    fn manufactured_distress_can_never_reach_zero_dial_share() {
        let mut record = ConductRecord::neutral();
        for _ in 0..1000 {
            record = observe(record, ConductEvidence::NonPerformance, 0);
        }
        assert!(
            dial_share(record) >= MIN_NON_PERFORMANCE_DIAL_SHARE,
            "sustained manufactured distress drove the share to {}",
            dial_share(record)
        );
        assert!(dial_share(record) > 0.0);
    }

    /// A proven lie MAY exclude durably — the one case SPEC §8.3 permits. Paired with the test above
    /// so the two directions are pinned together: testing only the floor would pass for an
    /// implementation that could never exclude anyone at all.
    #[test]
    fn a_proven_lie_may_exclude_where_non_performance_may_not() {
        let liar = observe(ConductRecord::neutral(), ConductEvidence::ProvenLie, 0);
        assert_eq!(dial_share(liar), 0.0);
        assert_eq!(
            dial_share(decay(liar, u64::MAX)),
            0.0,
            "a proven fault does not decay"
        );
    }

    /// SPEC §8.2A: silence must not be cheaper than answering honestly. A peer that answers "I do not
    /// have it" must not rank below one that says nothing — the honest answerer here is compared
    /// against a silent peer over the SAME number of interactions.
    #[test]
    fn answering_honestly_never_ranks_below_staying_silent() {
        let mut honest = ConductRecord::neutral();
        let mut silent = ConductRecord::neutral();
        for _ in 0..4 {
            honest = observe(honest, ConductEvidence::HonestAnswer, 0);
            silent = observe(silent, ConductEvidence::NonPerformance, 0);
        }
        assert!(dial_share(honest) > dial_share(silent));
    }

    /// An honest answer must not RAISE a peer's standing either — otherwise a peer talks its way up,
    /// which is the promote-on-declaration SPEC §8.1 forbids. Starting from a penalised state, honest
    /// answers at a frozen tick must not repair it; only time may.
    #[test]
    fn an_honest_answer_cannot_repair_a_penalty_without_time_passing() {
        let penalised = observe(ConductRecord::neutral(), ConductEvidence::NonPerformance, 0);
        let after_talking = observe(penalised, ConductEvidence::HonestAnswer, 0);
        assert_eq!(after_talking.non_performance, penalised.non_performance);
    }

    /// SPEC §8.2A persistence: an unreadable record falls back to NEUTRAL, never to penalised.
    /// Losing a file must not become an exclusion.
    #[test]
    fn the_fallback_for_an_unreadable_record_is_neutral() {
        assert_eq!(dial_share(ConductRecord::neutral()), 1.0);
    }

    /// SPEC §8.4: state keyed by an attacker-supplied identity is bounded. Unbounded accumulation
    /// would also make the penalty arbitrarily deep rather than merely capped.
    #[test]
    fn the_transient_penalty_is_bounded() {
        let mut record = ConductRecord::neutral();
        for _ in 0..10_000 {
            record = observe(record, ConductEvidence::NonPerformance, 0);
        }
        assert_eq!(record.non_performance, NON_PERFORMANCE_CEILING);
    }
}
