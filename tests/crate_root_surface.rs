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
    in_keyspace_neighbourhood, may_displace, merge_answers, observe, parse_enabled, reconcile,
    select_within_capacity, should_displace, xor_proximity, AcquisitionDecision, BackfillPolicy,
    CacheTier, CapsuleIdentity, ConductEvidence, ConductRecord, DisplacementMargin,
    ForwardDecision, ForwardRefusal, InboundAsk, Provenance, RecursionConfig, RelevanceValue,
    SelectionCandidate, SelectionPolicy, SelectionSeed, MIN_DISPLACEMENT_MARGIN,
    MIN_NON_PERFORMANCE_DIAL_SHARE, NON_PERFORMANCE_CEILING, NON_PERFORMANCE_DECAY_TICKS,
    NON_PERFORMANCE_PENALTY,
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

/// SPEC §3.2/§8.5, resolved the way a consumer resolves it: the displacement margin is reachable
/// and IN FORCE through `select_within_capacity` — the one selection entry point — without the
/// caller naming it.
///
/// This is an integration test for the same reason the file is: the defect it guards was that every
/// piece of the defence existed and none of it was on the path a consumer walks. A unit test inside
/// `selection` proves the rule; only this proves a consumer gets it.
#[test]
fn the_displacement_margin_is_in_force_through_the_root_selection_api() {
    let contest = |challenger_score: f64| {
        let contenders = [
            SelectionCandidate {
                id: b'i',
                tier: CacheTier::Tier1Demand,
                size_bytes: 100,
                score: RelevanceValue(1.0),
                pinned: false,
                resident: true,
            },
            SelectionCandidate {
                id: b'c',
                tier: CacheTier::Tier1Demand,
                size_bytes: 100,
                score: RelevanceValue(challenger_score),
                pinned: false,
                resident: false,
            },
        ];
        // The policy a consumer writes: a capacity and a node-local seed. Nothing here mentions a
        // margin, and the margin is nonetheless applied.
        let policy = SelectionPolicy::new(100, SelectionSeed::from_peer_id(&[7u8; 32]));
        select_within_capacity(&contenders, policy).retained
    };

    assert_eq!(
        contest(1.0 + MIN_DISPLACEMENT_MARGIN / 2.0),
        vec![b'i'],
        "a default-constructed policy must still refuse a marginal displacement"
    );
    assert_eq!(
        contest(1.0 + MIN_DISPLACEMENT_MARGIN * 2.0),
        vec![b'c'],
        "and must still admit a challenger that clears the margin"
    );

    // The margin can be raised and never lowered, through the type a consumer configures.
    assert_eq!(
        SelectionPolicy::new(100, SelectionSeed::from_node_local(1))
            .with_margin(DisplacementMargin::new(0.0))
            .margin()
            .get(),
        MIN_DISPLACEMENT_MARGIN
    );
    assert!(may_displace(
        RelevanceValue(0.0),
        RelevanceValue(1.0),
        DisplacementMargin::default()
    ));
}
