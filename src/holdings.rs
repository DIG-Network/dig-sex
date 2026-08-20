//! Holdings — every eviction is an advertising retraction (SPEC §5).
//!
//! A node advertises what it holds, so that other nodes can find it. The corollary is easy to drop
//! and expensive to drop: **a capsule the cache evicted is one the node MUST stop advertising.** A
//! node that keeps advertising evicted content spends other nodes' dial budget on a guaranteed miss,
//! and it does so silently — the advertiser sees nothing wrong, and only the dialler pays.
//!
//! The advertisement therefore FOLLOWS the cache, never the other way round. `dig-store-cache`
//! already emits the signal this needs: `Admission.evicted` names exactly what left, and `holdings()`
//! is its complement. This module turns those into the announce/retract pair the DHT and the
//! holdings announcement consume, so the coupling is expressed once instead of at each call site that
//! happens to remember it.
//!
//! Announcing is not this crate's job either — it produces the DELTA and the caller applies it.

use dig_store_cache::CapsuleIdentity;

/// What must be announced and what must be retracted after a cache operation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HoldingsDelta {
    /// Newly held — announce these as provided.
    pub announce: Vec<CapsuleIdentity>,
    /// No longer held — retract these. A retraction MUST NOT be skipped as an optimisation: the cost
    /// of a stale advertisement falls on other nodes, so it is invisible locally.
    pub retract: Vec<CapsuleIdentity>,
}

impl HoldingsDelta {
    /// Whether the delta changes anything. A no-op delta is common (most admissions evict nothing)
    /// and the caller should not pay for a network round when nothing moved.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.announce.is_empty() && self.retract.is_empty()
    }
}

/// The delta produced by admitting `admitted` and evicting `evicted` — `dig-store-cache`'s
/// `Admission.evicted` fed straight in.
///
/// An admission that evicted the very capsule it admitted would be self-cancelling; that cannot
/// happen (the cache excludes the incoming capsule from its own eviction candidates), and this
/// function does not paper over it — it reports what the cache reported.
#[must_use]
pub fn after_admission(admitted: CapsuleIdentity, evicted: &[CapsuleIdentity]) -> HoldingsDelta {
    HoldingsDelta {
        announce: vec![admitted],
        retract: evicted.to_vec(),
    }
}

/// The delta produced by an eviction-only sweep — a reconfigure to a smaller capacity, or a
/// post-land sweep. Nothing arrived, so nothing is announced.
#[must_use]
pub fn after_eviction(evicted: &[CapsuleIdentity]) -> HoldingsDelta {
    HoldingsDelta {
        announce: Vec::new(),
        retract: evicted.to_vec(),
    }
}

/// Reconcile what is currently advertised against what is actually held.
///
/// This is the repair path, for a node whose advertisements have drifted from its cache — after a
/// crash between an eviction and its retraction, or after a rebuild that recovered a different set
/// from disk. Incremental deltas are the normal path; this is what makes a missed one recoverable
/// rather than permanent.
#[must_use]
pub fn reconcile(advertised: &[CapsuleIdentity], held: &[CapsuleIdentity]) -> HoldingsDelta {
    HoldingsDelta {
        announce: held
            .iter()
            .filter(|capsule| !advertised.contains(capsule))
            .copied()
            .collect(),
        retract: advertised
            .iter()
            .filter(|capsule| !held.contains(capsule))
            .copied()
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capsule(byte: u8) -> CapsuleIdentity {
        CapsuleIdentity {
            store_id: [byte; 32].into(),
            root_hash: [byte; 32].into(),
        }
    }

    /// The rule the whole module exists for: an admission that evicted something produces BOTH an
    /// announce and a retract. Producing only the announce is the natural implementation and the one
    /// that leaves other nodes dialling for content this node no longer has.
    #[test]
    fn an_admission_that_evicts_announces_and_retracts_in_the_same_delta() {
        let delta = after_admission(capsule(1), &[capsule(2), capsule(3)]);
        assert_eq!(delta.announce, vec![capsule(1)]);
        assert_eq!(delta.retract, vec![capsule(2), capsule(3)]);
    }

    #[test]
    fn an_admission_that_evicts_nothing_retracts_nothing() {
        let delta = after_admission(capsule(1), &[]);
        assert_eq!(delta.announce, vec![capsule(1)]);
        assert!(delta.retract.is_empty());
        assert!(!delta.is_empty(), "an announce alone is still a change");
    }

    #[test]
    fn a_sweep_retracts_without_announcing() {
        let delta = after_eviction(&[capsule(2)]);
        assert!(delta.announce.is_empty());
        assert_eq!(delta.retract, vec![capsule(2)]);
    }

    /// Reconciliation must repair drift in BOTH directions — a stale advertisement to retract AND a
    /// held capsule never announced. A one-directional implementation passes a retract-only fixture,
    /// so both are asserted from one state.
    #[test]
    fn reconcile_repairs_drift_in_both_directions() {
        let advertised = [capsule(1), capsule(2)];
        let held = [capsule(2), capsule(3)];

        let delta = reconcile(&advertised, &held);

        assert_eq!(
            delta.retract,
            vec![capsule(1)],
            "advertised but not held must be retracted"
        );
        assert_eq!(
            delta.announce,
            vec![capsule(3)],
            "held but not advertised must be announced"
        );
    }

    #[test]
    fn reconcile_of_a_consistent_node_is_a_no_op() {
        let both = [capsule(1), capsule(2)];
        assert!(reconcile(&both, &both).is_empty());
    }
}
