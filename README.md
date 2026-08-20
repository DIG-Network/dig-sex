# dig-sex — Store EXchange

[![crates.io](https://img.shields.io/crates/v/dig-sex.svg)](https://crates.io/crates/dig-sex)

The decision layer for exchanging DIG stores between peers. It answers **which** store a node holds,
**in what order** it sheds them, **which peers** are worth dialling, and **what** to do with an
inbound request.

It decides. It does not act.

```toml
[dependencies]
dig-sex = "0.2"
```

`SPEC.md` is the normative contract; this README is how to use it. Where the two disagree, the SPEC
wins.

---

## The one thing to understand first

**Every function here is pure.** No clocks, no sockets, no filesystem, no global state. A tick is a
number you pass in; a peer is a value you supply; capacity is an argument.

That is not stylistic. It is what lets the same decisions be unit-tested exhaustively, reproduced
from a log, and reasoned about without a running network. **The caller owns all I/O and all
persistence** — you keep the ledgers, hold the sockets, read the clock, and call in at the decision
points.

The practical consequence: *importing this crate does nothing*. It is wired in only when a real path
calls it.

---

## The objective function

Two goals, in strict order:

1. **Profit first.** Never sacrifice paid content to hold unpaid content.
2. **Then the largest NUMBER of mirrors** that fits the disk allocation.

**Lexicographic, never weighted.** There is no exchange rate between them and an implementation must
not invent one.

Because the second objective is a *count*, selection is a **knapsack**: size is the weight, score is
the value, and within a tier the crate fills **smallest-first**. Sorting by score and filling until
full is the named wrong answer — it maximises retained score, which is not what was asked for.

---

## The tier ladder

```rust
use dig_sex::CacheTier;

CacheTier::Tier0Precache; // speculative — sacrificed FIRST
CacheTier::Tier1Demand;   // someone actually read it — sacrificed only after all Tier0
CacheTier::Tier2Bribed;   // paid retention — sacrificed LAST
```

**Cross-tier precedence is absolute.** A score orders capsules *within* a tier and never across one.
A perfect Tier0 score never outranks a poor Tier1. That is what makes a capsule a user really read
outlive one the node fetched on a hunch.

A capsule's tier is the **maximum** across every source that claims it:

```rust
use dig_sex::{effective_tier, CacheTier};

let tier = effective_tier([CacheTier::Tier0Precache, CacheTier::Tier1Demand]);
assert_eq!(tier, Some(CacheTier::Tier1Demand)); // demand promotes a precached store
```

---

## Pluggable algorithms

A tier source implements one method:

```rust
use dig_sex::{AlgorithmSet, CacheTier, ExchangeAlgorithm, RelevanceValue, StoreFacts};

struct DemandLedger { /* your state */ }

impl ExchangeAlgorithm<MyId> for DemandLedger {
    fn facts(&self, id: &MyId) -> Option<StoreFacts> {
        // `None` means "no opinion" — NOT a demotion. It removes this source's claim and
        // leaves the others to answer, so a promotion survives one reason lapsing.
        self.read_count(id).map(|_| StoreFacts {
            tier: CacheTier::Tier1Demand,
            score: RelevanceValue(0.0),
        })
    }
}

let algorithms = AlgorithmSet::new()
    .with(Box::new(DemandLedger { /* .. */ }))
    .with(Box::new(SidecarTags { /* .. */ }));

let facts = algorithms.facts_or_default(&id); // unclaimed → DEFAULT_TIER, protected
```

Composition is a **maximum**, so registration order cannot change policy. The score comes from the
**winning tier's** claim, never the global maximum — a Tier0 desirability must not follow a store
another source promoted.

**A paid-retention algorithm is one more registration.** Demotion on non-payment rides the same
channel: the algorithm stops claiming, its claim drops out of the maximum, and the store falls back
to whatever the others say. No second mechanism, no private state.

---

## Selection

```rust
use dig_sex::{select_within_capacity, CacheTier, RelevanceValue, SelectionCandidate, SelectionSeed};

let candidates = vec![SelectionCandidate {
    id: capsule_id,
    tier: CacheTier::Tier1Demand,
    size_bytes: 4_000_000,
    score: RelevanceValue(0.71),
    pinned: false, // a pin is retained unconditionally and MAY exceed capacity
}];

// The seed must be node-local and NOT derivable by a peer, or a peer could arrange to win
// every tie on every node at once.
let seed = SelectionSeed::from_peer_id(&own_peer_id);

let outcome = select_within_capacity(&candidates, capacity_bytes, seed);
outcome.retained; // highest tier first, then smallest first within a tier
outcome.rejected; // in EVICTION order — the first entry is sacrificed first
```

`rejected` is selection order reversed, so the two answers are consistent by construction rather
than by a second sort that could drift.

`SelectionSeed::from_node_local(value)` exists for the window before a node knows its own peer id.
Be aware of what it costs: in that window the relevance score is a function of a `peer_id` that does
not exist yet, so **every capsule scores zero and the constant becomes the sole tiebreak** — every
un-brought-up node then breaks ties identically. They agree on what to *evict*, not on what to
*acquire*, so it cannot concentrate the network's mirrors, and it ends when the identity arrives.

---

## Eviction, through the cache's own seam

`dig-store-cache` has shipped a pluggable `EvictionPolicy` trait since v0.1.0 whose only
implementation is LRU. `TieredPolicy` is the first non-LRU one:

```rust
use std::sync::Arc;
use dig_sex::TieredPolicy;
use dig_store_cache::EvictionPolicy;

let policy = TieredPolicy::new(Arc::new(algorithms), seed);
let victims = policy.select_evictions(&eviction_context);
```

**`TieredPolicy` deliberately never reads `EvictionEntry::last_access`.** `dig-store-cache` bumps
that stamp inside `Cache::get`, and `get` is the same call the **serving** path makes for an inbound
peer request — so on a serving node it is an attacker-chosen value, and eviction order becomes a
thing peers vote on. Recency still influences eviction, but only through the relevance score, whose
`reads_recency_ticks` input **you must attribute to LOCAL reads only**.

If you pass inbound-driven counts into `reads_recency_ticks`, you reintroduce the defect by the
other route.

---

## Peer conduct

Classified by **how** a peer failed, because the classes are not equally knowable:

```rust
use dig_sex::{conduct, ConductEvidence, ConductRecord};

// Verifiable, and may persist:
//   ProvenLie          — bytes that failed verification against the chain-anchored root
//   SelfContradiction  — claimed to hold it, then answered absence
//
// NOT verifiable, and must decay:
//   NonPerformance     — timeout, reset, silence, truncation, slowness
//   HonestAnswer       — credits nothing; it is the absence of a fault, not a reward

let record = conduct::observe(record, ConductEvidence::NonPerformance, now_ticks);
let share = conduct::dial_share(record); // 0.0 for a proven fault; never below 0.1 otherwise
```

Non-performance **decays on elapsed time alone** and can never reduce a dial share to zero, because
**an attacker can manufacture distress in an honest third party**. Punishing that permanently
punishes the victim.

**Reputation is local and MUST NOT be gossiped.** A shared reputation channel is a defamation
primitive.

---

## Admission — refuse before you work

```rust
use dig_sex::{AdmissionLimits, AdmissionMeter, AuthenticatedPeer, Refusal, WorkKind};

let mut meter = AdmissionMeter::new(AdmissionLimits { /* .. */ });

// The identity MUST come from the verified session, never from the request body.
let peer = AuthenticatedPeer::from_verified_session(session_peer_id);

match meter.admit(peer, WorkKind::Own, requested_units) {
    Ok(()) => {
        let result = do_the_work();
        meter.release(peer, WorkKind::Own); // on EVERY exit path, including errors
        result
    }
    Err(Refusal::PeerShare) => refuse(),
    Err(other) => refuse_with(other),
}
```

Two ways to get this wrong, both of which look like working protection:

- **Calling `admit` after the expensive part.** Then it is decoration.
- **Passing a placeholder identity.** That collapses every requestor into one shared bucket, turning
  a per-peer limit into a global one — so a single peer exhausts the allowance for everybody.

A `release` skipped on an error path leaks allowance until the node refuses everything.

---

## Recursive discovery

A node asked for a store it does not hold can ask **its own** peers, rather than answering a bare
absence. Every node's peer set is a narrow, arbitrary slice of the network, so an answer this node
cannot give may be one hop away — which a global provider index cannot substitute for, because it
cannot reach a holder it has never heard of.

```rust
use dig_sex::{discovery, ForwardDecision, InboundAsk, RecursionConfig};

let config = RecursionConfig {
    enabled: discovery::parse_enabled(std::env::var("DIG_RECURSION").ok().as_deref()),
    ..RecursionConfig::default()
};

// State the real cost rather than leaving it to be discovered in production:
config.worst_case_nodes_recruited(); // fan_out ^ hop_cap

match discovery::decide_forward(&config, &ask, &this_node, &known_peers, relay_budget_available) {
    ForwardDecision::Forward { peers, hops_remaining } => ask_each(peers, hops_remaining),
    ForwardDecision::Refuse(why) => answer_absence(why),
}
```

**Reach is `fan_out ^ hop_cap` — an exponent, not a knob.** A concurrency ceiling is not a
substitute: it bounds how many hops happen *at once*, never how many happen.

**Off by default, and unrecognised config fails closed.** This path spends *other* nodes' bandwidth,
so it must not be gated more loosely than one spending only your own, and a typo must never be able
to enable a network-wide amplifier. Use `parse_enabled` rather than your own parse.

**A disabled node forwards nothing, including relaying for others** — the switch cuts the chain at
every node, not only at originators.

Answers relayed by a hop are **hearsay**:

```rust
let merged = discovery::merge_answers(&config, &first_hand, &hearsay);
// -> Vec<(Answer, Provenance)>
```

Hearsay is a candidate for the **fetch** path, where verification makes a wrong candidate merely
wasted work. It must never be asserted onward as this node's own knowledge, and the answer cap falls
on the **forwarded** portion only — otherwise one peer returning a slate of fabricated holders
evicts every genuine holder from the answer, for free.

---

## Holdings — an eviction is a retraction

```rust
use dig_sex::holdings;

let delta = holdings::after_eviction(&victims);
let delta = holdings::after_admission(admitted, &evicted);
let delta = holdings::reconcile(&advertised, &held); // drift repair, e.g. after an unclean shutdown

if !delta.is_empty() {
    announce(&delta.announce);
    retract(&delta.retract);
}
```

**A retraction must not be skipped as an optimisation.** The cost of a stale advertisement falls on
*other* nodes — they dial you for content you deleted — so it is invisible locally and free to get
wrong.

---

## Acquisition — a read creates relevance

```rust
use dig_sex::{acquisition, AcquisitionDecision, BackfillPolicy};

match acquisition::decide(policy, &capsule, already_held, &in_flight) {
    AcquisitionDecision::Acquire => start_background_backfill(),
    AcquisitionDecision::SkipAlreadyHeld
    | AcquisitionDecision::SkipInFlight
    | AcquisitionDecision::SkipDisabled => {}
}
```

Distinct from speculative precache: this is **demand-driven**. A node that served one piece of
content has a reason to mirror the whole store, because that request created relevance.

---

## Reward accounting

```rust
use dig_sex::{RecordOutcome, RewardClaim, RewardLedger};

let mut ledger = RewardLedger::from_chain_claims(claims_from_chain);

match ledger.record(claim) {
    RecordOutcome::Recorded => persist(&ledger),
    RecordOutcome::AlreadyRecorded => {} // replay: the total did NOT move
}

ledger.claimed_for(&store);
ledger.reconcile_from_chain(fresh_claims); // chain overwrites local belief, never the reverse
```

**Pure and in-memory — you own persistence and durability.** Three properties must survive your
storage layer:

- **Chain is authoritative.** `reconcile_from_chain` exists so on-chain truth overwrites local
  belief, never the reverse.
- **Recording is idempotent**, keyed on the claim id. If you key rows on insertion order instead, a
  replayed chain event inflates the total and the crate's guarantee is gone.
- **It fails toward UNDER-counting.** When unsure, report less claimed, never more. Over-reporting
  claimed rewards is a surface that lies about money.

The **claim mechanism** — how a node actually claims from an on-chain distributor — is deliberately
not specified yet.

---

## Status

**Published and in production.** dig-node 0.130.0 uses this crate for cache retention: relevance
scoring, tier composition, selection, and eviction through `TieredPolicy`.

Implemented and tested, but **not yet wired into any node**: `discovery`, `conduct`, `admission`,
`acquisition`, `reward`. They work; nothing calls them yet. Tracked in
[dig_ecosystem#3138](https://github.com/DIG-Network/dig_ecosystem/issues/3138).

## License

MIT OR Apache-2.0.
