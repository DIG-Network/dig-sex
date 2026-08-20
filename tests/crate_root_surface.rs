//! SPEC §11A: everything the public-API section names is reachable from the crate root.
//!
//! This is an integration test rather than a unit test on purpose: it resolves names through the
//! crate's EXTERNAL root, which is the path a consumer actually uses. A unit test would resolve
//! `crate::relevance::xor_proximity` whether or not `lib.rs` re-exported it, and so could not fail
//! for the defect this file exists to catch.
//!
//! Every name below is a compile-time assertion. Deleting a re-export from `lib.rs` breaks this
//! build; the test bodies exist only to give the imports a use.

use dig_sex::{
    after_admission, after_eviction, decay, decide, decide_forward, dial_share,
    in_keyspace_neighbourhood, merge_answers, observe, parse_enabled, reconcile, should_displace,
    xor_proximity, AcquisitionDecision, BackfillPolicy, CapsuleIdentity, ConductEvidence,
    ConductRecord, ForwardDecision, ForwardRefusal, InboundAsk, Provenance, RecursionConfig,
    RelevanceValue, MIN_DISPLACEMENT_MARGIN, MIN_NON_PERFORMANCE_DIAL_SHARE,
    NON_PERFORMANCE_CEILING, NON_PERFORMANCE_DECAY_TICKS, NON_PERFORMANCE_PENALTY,
};

use std::collections::HashSet;

fn capsule(byte: u8) -> CapsuleIdentity {
    CapsuleIdentity {
        store_id: [byte; 32].into(),
        root_hash: [byte; 32].into(),
    }
}

/// The keyspace and displacement helpers `dig-node` reaches for, which it had to import by module
/// path while the root omitted them.
#[test]
fn relevance_helpers_are_reachable_from_the_root() {
    let id = [0u8; 32];
    assert!((xor_proximity(&id, &id) - 1.0).abs() < f64::EPSILON);
    assert!(in_keyspace_neighbourhood(&id, &id));
    assert!(should_displace(
        RelevanceValue(0.0),
        RelevanceValue(1.0),
        MIN_DISPLACEMENT_MARGIN
    ));
}

/// The three holdings functions and the acquisition decision, which together make an eviction a
/// retraction at the caller (SPEC §7.1).
#[test]
fn holdings_and_acquisition_are_reachable_from_the_root() {
    let held = capsule(1);
    assert!(after_admission(held, &[]).retract.is_empty());
    assert_eq!(after_eviction(&[held]).retract, vec![held]);
    assert_eq!(reconcile(&[], &[held]).announce, vec![held]);
    assert_eq!(
        decide(BackfillPolicy::default(), &held, true, &HashSet::new()),
        AcquisitionDecision::SkipAlreadyHeld
    );
}

/// The conduct surface: three functions and the four bounds that keep non-performance recoverable.
#[test]
fn conduct_surface_is_reachable_from_the_root() {
    let record = observe(ConductRecord::neutral(), ConductEvidence::NonPerformance, 0);
    assert_eq!(record.non_performance, NON_PERFORMANCE_PENALTY);
    assert!(dial_share(record) >= MIN_NON_PERFORMANCE_DIAL_SHARE);
    let at_ceiling = ConductRecord {
        non_performance: NON_PERFORMANCE_CEILING,
        ..record
    };
    assert!((dial_share(at_ceiling) - MIN_NON_PERFORMANCE_DIAL_SHARE).abs() < f64::EPSILON);
    assert_eq!(
        decay(record, NON_PERFORMANCE_DECAY_TICKS).non_performance,
        0
    );
}

/// The three discovery functions, including the fail-closed setting parser.
#[test]
fn discovery_functions_are_reachable_from_the_root() {
    assert!(!parse_enabled(Some("yes please")));

    // Recursion defaults OFF (SPEC §6.1), so a default config refuses before reading anything else.
    let config = RecursionConfig::default();
    let ask = InboundAsk {
        requestor: 1u8,
        hops_remaining: Some(4),
    };
    assert_eq!(
        decide_forward(&config, &ask, &2u8, &[3u8], true),
        ForwardDecision::Refuse(ForwardRefusal::Disabled)
    );
    assert_eq!(
        merge_answers(&config, &[9u8], &[]),
        vec![(9u8, Provenance::FirstHand)]
    );
}
