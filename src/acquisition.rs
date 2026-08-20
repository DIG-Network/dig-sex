//! Acquisition — a read creates relevance (SPEC §4.1).
//!
//! When a read for a `(store_id, root)` is satisfied **from another node**, the node acquires the
//! WHOLE `.dig` capsule for that generation in the background, so the next read is served locally. A
//! one-off remote read becomes a durable local copy without the store being subscribed, and that is
//! the flywheel: every read makes content more available.
//!
//! The request itself is the evidence of relevance, so an acquisition earns
//! [`Tier1Demand`](crate::tier::CacheTier::Tier1Demand) — never
//! [`Tier0Precache`](crate::tier::CacheTier::Tier0Precache), which is reserved for content nobody
//! asked for.
//!
//! # This module decides; it does not fetch
//!
//! [`decide`] is a pure function returning [`AcquisitionDecision`]. The caller performs the pull, and
//! MUST do so **without blocking the read that triggered it** — the triggering read is already
//! answered and must never be delayed by a warm-up behind it.
//!
//! # Acquisition is not admission (SPEC §4.2)
//!
//! [`AcquisitionDecision::Acquire`] says a capsule is WORTH fetching. It says nothing about whether
//! the bytes that arrive are valid. Verification against the chain-anchored root belongs to the fetch
//! path and is deliberately not re-implemented here; a tier is a policy statement, never a claim
//! about content.

use std::collections::HashSet;

use dig_store_cache::CapsuleIdentity;

/// Whether read-triggered acquisition runs. Default ON: the flywheel is the point, and a node that
/// reads remotely without warming up stays a permanent leech.
///
/// Modelled as a struct rather than a bare `bool` so the default is expressed once, here, instead of
/// at each call site where it could drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackfillPolicy {
    /// `false` disables read-triggered acquisition entirely.
    pub enabled: bool,
}

impl Default for BackfillPolicy {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// What the caller should do about a read that was satisfied remotely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcquisitionDecision {
    /// Pull the whole capsule in the background, admitting it as `Tier1Demand`.
    Acquire,
    /// Read-triggered acquisition is switched off.
    SkipDisabled,
    /// The capsule is already cached — there is nothing to warm up.
    SkipAlreadyHeld,
    /// A pull for this exact `store:root` is already running. A burst of reads for the same
    /// not-yet-held store MUST produce ONE pull, not one per read.
    SkipInFlight,
}

/// Decide whether a remotely-satisfied read should trigger a whole-capsule acquisition.
///
/// The order of the guards is the order of their cost: the switch is free, the held check is a cache
/// lookup, and the in-flight check is the one that needs shared state. Reporting the FIRST reason
/// that applies also keeps the answer stable — a caller that logs it gets one reason, not a set.
#[must_use]
pub fn decide(
    policy: BackfillPolicy,
    capsule: &CapsuleIdentity,
    already_held: bool,
    in_flight: &HashSet<CapsuleIdentity>,
) -> AcquisitionDecision {
    if !policy.enabled {
        return AcquisitionDecision::SkipDisabled;
    }
    if already_held {
        return AcquisitionDecision::SkipAlreadyHeld;
    }
    if in_flight.contains(capsule) {
        return AcquisitionDecision::SkipInFlight;
    }
    AcquisitionDecision::Acquire
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capsule(store: u8, root: u8) -> CapsuleIdentity {
        CapsuleIdentity {
            store_id: [store; 32].into(),
            root_hash: [root; 32].into(),
        }
    }

    fn nothing_in_flight() -> HashSet<CapsuleIdentity> {
        HashSet::new()
    }

    #[test]
    fn a_remote_read_of_an_unheld_capsule_triggers_acquisition_by_default() {
        assert_eq!(
            decide(
                BackfillPolicy::default(),
                &capsule(1, 1),
                false,
                &nothing_in_flight()
            ),
            AcquisitionDecision::Acquire
        );
    }

    #[test]
    fn the_switch_and_the_held_check_both_suppress_acquisition() {
        assert_eq!(
            decide(
                BackfillPolicy { enabled: false },
                &capsule(1, 1),
                false,
                &nothing_in_flight()
            ),
            AcquisitionDecision::SkipDisabled
        );
        assert_eq!(
            decide(
                BackfillPolicy::default(),
                &capsule(1, 1),
                true,
                &nothing_in_flight()
            ),
            AcquisitionDecision::SkipAlreadyHeld
        );
    }

    /// SPEC §4.1: dedup is keyed on `store:root`, not on the store. A burst of reads for the same
    /// generation collapses to one pull, but a read for a DIFFERENT generation of the same store is a
    /// different capsule and must still be acquired — a store-keyed dedup passes the first assertion
    /// and silently fails the second, permanently stranding every node on the first root it saw.
    #[test]
    fn dedup_is_keyed_on_the_generation_not_the_store() {
        let mut in_flight = HashSet::new();
        in_flight.insert(capsule(1, 1));

        assert_eq!(
            decide(BackfillPolicy::default(), &capsule(1, 1), false, &in_flight),
            AcquisitionDecision::SkipInFlight
        );
        assert_eq!(
            decide(BackfillPolicy::default(), &capsule(1, 2), false, &in_flight),
            AcquisitionDecision::Acquire,
            "a newer generation of the same store is a different capsule"
        );
    }

    /// A failed acquisition must be retryable: releasing the in-flight slot restores the `Acquire`
    /// answer, so a transient failure does not permanently suppress warm-up for that generation.
    #[test]
    fn releasing_the_in_flight_slot_makes_the_capsule_acquirable_again() {
        let mut in_flight = HashSet::new();
        in_flight.insert(capsule(1, 1));
        in_flight.remove(&capsule(1, 1));

        assert_eq!(
            decide(BackfillPolicy::default(), &capsule(1, 1), false, &in_flight),
            AcquisitionDecision::Acquire
        );
    }
}
