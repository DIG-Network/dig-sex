# dig-sex — normative specification

The authoritative contract for the DIG store-exchange layer. An independent implementation built against
this document alone must interoperate with, and be substitutable for, the reference one.

Requirement levels are RFC 2119. Where a clause records a defect this ecosystem has already measured, the
reason is stated with it — those reasons are normative context, not commentary, because each is a rule
someone previously violated by accident.

---

## 0. The objective function

**Optimise for PROFIT first. Subject to that, MAXIMISE THE NUMBER OF MIRRORS within the disk allocation.**

The two are **lexicographic, not weighted**. No quantity of additional mirrors justifies sacrificing paid
retention, there is no exchange rate between them, and an implementation MUST NOT introduce one.

Everything in this document is machinery for that sentence. The tier ladder (§2) implements the primary
objective; the score (§3) and the selection (§4) implement the secondary objective within whatever
capacity the primary leaves.

### 0.1 The secondary objective is a COUNT, so size is first-class

*Maximise mirrors* means maximise the **number of stores mirrored** — not bytes held, not aggregate
relevance retained. **All else equal, many small stores beat one large store**, because each mirror is a
unit of network usefulness regardless of its size.

Therefore **size MUST NOT enter the relevance score**. Score is **value**, size is **weight**, and mixing
them destroys the ability to select against a capacity bound.

### 0.2 Deliberately unspecified

An implementation MUST NOT settle either of these by accident:

1. **Whether profit is merely honoured or actively sought.** Retaining what has been paid for is
   unambiguous. Acquiring content because payment is *expected* is a different behaviour with different
   failure modes, and is not specified here.
2. **What counts as profit** — the accounting unit, its proof, and what makes it non-repudiable — belongs
   with the paid-retention algorithm (§2.4).

Until both are answered, the primary objective means exactly **"never sacrifice paid content to hold
unpaid content"** and no more.

---

## 1. Scope

`dig-sex` owns the **policy** of the full store-exchange lifecycle: **requesting**, **delivering**, the
**recursive discovery** between them, and the **cache/tier/relevance system** deciding what a node holds.

It answers: which store to acquire, from whom, what to keep, what to sacrifice first, when a candidate is
worth displacing an incumbent, and what to do when a peer asks for something this node does not hold.

### 1.1 What this crate is NOT

It MUST NOT become a transport, a discovery mechanism, a fetcher, or a verifier:

| concern | owner | this crate's part |
|---|---|---|
| provider discovery, announce/retract | `dig-dht` | decides *when* to announce and retract |
| peer records, gossip | `dig-pex` | decides *which* peers to prefer |
| byte movement, ranges, resume | `dig-download` | decides *what* to fetch and *from whom* |
| on-disk staging, fsync, rename, rebuild | `dig-store-cache` | decides *what to admit and evict* |
| merkle / chain-anchor verification | the caller | MUST NOT re-implement |

**The dividing line is mechanism versus policy.** *"Stage to a temp file, fsync, rename"* is mechanism.
*"Which store, from whom, and what do I drop to make room"* is policy.

### 1.2 Dependencies

`dig-sex` sits at **40-application**, so it may depend on any lower level and MUST NOT be depended upon by
one. Every crate below is an existing ecosystem crate; **this crate MUST use them rather than
re-implement their behaviour** (§11.1). Versions are the levels present at the time of writing and MUST
be resolved to a single set across the graph (§11.3).

| crate | level | version | why |
|---|---|---|---|
| `dig-constants` | 00 | 0.9.0 | shared constants; MUST NOT hardcode a value that lives here |
| `dig-pex` | 00 | 0.1.1 | peer records and the first-hand/second-hand provenance distinction |
| `dig-peer-protocol` | 00 | 0.6.0 | the peer opcode surface the recursive ask rides (§6.1) |
| `dig-chainsource-interface` | 00 | 0.2.0 | chain reads for reconciling the reward ledger (§2A.2) |
| `dig-nat` | 10 | 0.19.0 | peer identity and transport types crossing composed APIs (§11.3) |
| `dig-capsule` | 10 | 0.5.0 | the capsule identity acquired whole on a read-triggered fetch (§5.1) |
| `dig-dht` | 20 | 0.12.0 | provider discovery, announce and **retract** (§7.1) |
| `dig-peer` | 20 | 0.10.0 | the peer abstraction the exchange dials |
| `dig-download` | 30 | 0.15.0 | byte movement, ranges, resume |
| `dig-peer-selector` | 30 | 0.9.0 | peer selection — **compose it, do not replace it** |
| `dig-store-cache` | 30 | 0.1.1 | on-disk admission/eviction mechanics, and the eviction-policy seam this crate implements (§11.1) |

**Deferred, pending the payment specification:** `dig-mirror-coin` (10, 0.3.0) locks $DIG as collateral to
advertise a mirror, and is the expected source of **stake evidence** for §2.4's paid-retention input. It
MUST NOT be wired until that specification exists, and stake MUST arrive as an input (§2.4.2), never be
asserted by this crate.

**Not a dependency of this crate:** `dig-logging` is required of DIG service **binaries**, not libraries.
This crate MUST NOT install a subscriber or a sink; it emits through the host's.

**`dig-evidence` MUST NOT be used.** It is an **L2 crate**, and this crate is not an L2 consumer. The
similarity of name is a trap: the **evidence** this specification refers to — §2.4's demotion channel and
§8.2A's peer-conduct classes — is **this crate's own local, peer-scoped type**, describing what a peer did
to this node. It is not on-chain L2 evidence and MUST NOT be conflated with it, nor reuse its types to
borrow their authority.

### 1.3 Purity

The decision core MUST be **pure and deterministic**: no clock, no network, no filesystem, no ambient
randomness. Time enters only as caller-supplied monotonic tick counters; randomness only as a
caller-supplied seed (§4.4).

**This is load-bearing, not stylistic.** An exchange-policy regression is otherwise invisible — content
still arrives, just slower and from worse peers — so every decision MUST be replayable and auditable
offline from its recorded inputs.

---

## 2. The tier model

Every cacheable store holds exactly one **effective tier**.

| tier | rank | earned by | eviction precedence |
|---|---|---|---|
| `Tier0Precache` | 0 | speculative acquisition (neighbourhood precache) | sacrificed **FIRST** |
| `Tier1Demand` | 1 | a real read — local, or an inbound peer request | sacrificed after all `Tier0` |
| `Tier2Bribed` | 2 | a backer paid to keep it resident | sacrificed **LAST** |

### 2.1 Cross-tier precedence is absolute

**A relevance score MUST NOT move a store across tiers.** Across tiers, eviction precedence is fixed by
the tier alone. Within a tier, score and selection order candidates.

This makes the system a **capacity ladder**: higher tiers claim disk first, and **lower tiers occupy only
what higher tiers did not**. A `Tier0` entry is not *less relevant* than a `Tier1` entry — it is
**sacrificeable first regardless of relevance**, and that is deliberate.

### 2.2 Effective tier is the MAXIMUM across sources

A store MAY earn a tier by several routes at once; its effective tier is the **maximum**.

A promotion MUST survive the lapse of a lower reason: a store acquired speculatively and then read is
`Tier1Demand`, and MUST NOT fall back when the speculative reason expires while the read remains the more
recent fact.

An implementation MUST enumerate its tier sources explicitly. A source not enumerated cannot contribute to
the maximum, and silently omitting one demotes stores with no error.

### 2.3 Tier is persisted, and an unreadable tag fails SAFE

A store's tier MUST survive restart. A tag missing, truncated, or unrecognised MUST resolve to the
**protected default**, never the sacrificeable one — an unreadable tag MUST NOT cause eviction.

The persisted form SHOULD be a stable human-legible token rather than a numeric rank, so a future
renumbering cannot silently repoint existing tags.

### 2.4 `Tier2Bribed` exists; its algorithm is deferred

The paid tier is part of the model **now**. The algorithm deciding who pays, how much, and what proves it
is **deferred** and MUST NOT be invented here.

The seam MUST admit it later **without signature changes**. A paid-retention implementation MUST be able
to:

1. **read a price** — a value it cannot read is one it cannot price against;
2. **receive stake or payment evidence as an INPUT**, never assert it on the way out;
3. **demote a non-payer through the same evidence channel every other tier uses** — an algorithm forced
   to keep non-payment in private state has an interface that does not fit it;
4. **meter a MONEY budget distinct from a byte budget.**

A seam that cannot express all four does not conform, however well it expresses relevance.

---

## 2A. Reward accounting

`Tier2Bribed` is earned by payment, so this crate MUST maintain a **persistent, per-store record of
rewards claimed**. The node claims its rewards from an on-chain **reward distributor**; this crate tracks
what was claimed, for which store, and how much.

The claim MECHANISM — how a claim is constructed, submitted, and proven — is **deferred** and MUST NOT be
invented here. This section specifies the ledger it will write to, so that the mechanism can be added
without redesigning the store.

### 2A.1 The ledger is persistent and on disk

The record MUST survive process restart and MUST be durable against crash. A reward claimed and then lost
is money the operator earned and cannot see; a ledger that only exists in memory turns every restart into
an unnoticed loss.

### 2A.2 The chain is authoritative; the ledger is a local view

The ledger is a **cache of an on-chain fact**, never the fact itself. Where the two disagree, the chain
wins and the ledger MUST be correctable from it.

An implementation MUST be able to rebuild or reconcile the ledger from chain state. A local record that
cannot be checked against its source is not an accounting record.

### 2A.3 It MUST fail toward UNDER-counting

The two error directions are not symmetric and MUST NOT be treated as such:

- **Under-counting** — a claim made on chain but not recorded locally — costs the operator visibility of
  income they already hold. Recoverable by reconciliation (§2A.2).
- **Over-counting** — a claim recorded locally that did not occur, or was counted twice — makes this node
  **hold unpaid content as though it were paid**, sacrificing genuinely paid content to do so. That is a
  direct violation of §0's primary objective, and it is not self-correcting.

Therefore, where an implementation must choose, it MUST fail toward under-counting.

### 2A.4 Recording a claim MUST be idempotent

A claim MUST be identified by something derived from the chain event itself, so that replaying, retrying,
or re-observing it cannot double-count. A retry that inflates recorded profit is the over-counting failure
of §2A.3 arriving through the front door.

### 2A.5 The ledger is an INPUT to the decision core

§1.3 requires the decision core to be pure. The ledger is I/O, so **the core MUST NOT read or write it
directly**: claimed-reward figures enter the core as caller-supplied inputs, exactly as tick counters and
the tie-break seed do (§4.4).

This keeps every tier and eviction decision replayable offline from recorded inputs, and it keeps the
accounting testable without a chain.

### 2A.6 What this does NOT yet settle

Recording *what was claimed* is not the same as deciding *what a store is worth keeping for*. §0.2's open
question — whether profit is merely honoured or actively sought — is unaffected by this section, and the
per-store figures MUST NOT be interpreted as a purchase price, a bid, or a promise of future payment
without a specification saying so.

---

## 3. Relevance scoring

Within a tier, desirability is a bounded score.

- **The primary signal is XOR distance** between the content id and this node's peer id. Content landing
  near this node in the 256-bit keyspace is content this node is naturally responsible for.
- Around it sit **bounded, weighted bonuses**: replication scarcity (keep what few others hold), local
  demand (keep what our own users read), pin adjacency, and a large **pinned** bonus.
- Every bonus MUST be bounded. An unbounded term lets one input dominate, which is how a peer-supplied
  signal becomes a control channel.

### 3.1 Untrusted inputs MUST be clamped

Any input a peer can influence — a believed provider count above all — is **untrusted and potentially
flooded**, and MUST be clamped before reaching the score (§8).

### 3.2 Displacement requires a margin

A fresh candidate MUST NOT displace an incumbent on a marginally higher score; it MUST exceed the
incumbent by a configured **margin**. Without one, two near-equal stores evict each other repeatedly,
spending bandwidth on churn that produces no net change in what is held.

---

## 4. Selection

### 4.1 Selection is a per-tier knapsack over residual capacity

Score alone does not decide what is held. **Within a tier, selection maximises the NUMBER of stores held
against the capacity that tier is given** — score is the value, size is the weight, and the bound is
whatever higher tiers did not claim (§2.1).

A lower-scoring small store MAY be held over a higher-scoring large one **within a tier**. That is
correct, not a defect: it serves §0.1. It MUST NOT happen **across** tiers.

An implementation MAY approximate the knapsack. It MUST NOT degenerate into **sort-by-score-and-fill**,
which ignores the count objective entirely and is the obvious wrong implementation.

**Count-optimal is not score-optimal, and that is intended.** Filling smallest-first maximises the number
of stores held exactly, but among the sets of that same cardinality it does not maximise retained score;
a different set of equal count may score higher. This follows §0.1 — the objective is a count of mirrors,
"not aggregate relevance retained" — and is stated here so it is read as the contract rather than filed
as a defect.

### 4.2 Capacity

The node has a configured total allocation. Tiers claim it in descending rank order; each tier's bound is
the allocation minus what higher tiers claimed.

**Pins are an operator override**: a pinned entry MUST NOT be evicted and MAY push the node over its
allocation. An implementation MUST NOT silently re-evict to compensate.

### 4.3 Admission may exceed, or refuse

An implementation MAY admit over capacity when a policy returns too few victims; that is the caller's
explicit choice and MUST be reported, never silently corrected. An item larger than the whole allocation
MUST be refused rather than triggering an unsatisfiable eviction sweep.

### 4.4 Ties are broken RANDOMLY, from a seeded source

**Among candidates equal on profit, equal on size, and equal on score, selection MUST be random.**

**This is a network property, not a fairness gesture.** A deterministic tiebreak makes every node with a
similar view choose the *same* stores — a few mirrored by everyone, the rest by nobody — and aggregate
coverage is far worse than the same disk spent randomly. Randomising decorrelates independent nodes, the
only mechanism here producing even coverage without coordination.

Two constraints, both required:

- **The seed MUST be an INPUT**, drawn from node-local state, never ambiently inside the scorer. The same
  inputs including the seed MUST reproduce the same selection, preserving §1.3.
- **The seed MUST NOT be peer-derivable.** An attacker who can predict or influence it can bias which
  ties this node resolves in their favour, turning decorrelation into targeting. Node identity or local
  entropy is sound; content ids, provider counts, or anything a peer supplies is not.

**Randomise only among genuine ties.** Randomness MUST NOT reach across a profit, size, or score
difference — it is the last step, after §0's objectives have ordered everything they can.

**Score is part of the ordering, not merely of the value.** Selection is the only consumer of the score's
ordering power: tier is decided by §2 and cardinality by size, so a tiebreak that shuffled across a score
difference would leave §3's scoring model ordering nothing at all. §4.1 states that score is the value,
and a value that never orders anything is not one. Decorrelation is unharmed — §3's score is dominated by
XOR distance to **this node's own** peer id, so independent nodes already disagree on it, and the shuffle
still resolves the genuinely identical case.

---

## 5. Acquisition

### 5.1 A read creates relevance

When a read for a `(store_id, root)` is satisfied **from another node**, the node SHOULD acquire the
**whole** capsule for that generation in the background, so the next read is served locally. A one-off
remote read becomes a durable local copy **without** the store being subscribed.

This is `Tier1Demand` — the request itself is the evidence. It MUST be non-blocking, MUST NOT delay the
triggering read, and MUST deduplicate concurrent triggers for the same `store:root` into **one**
acquisition. It MUST be configurable, SHOULD default ON, and MUST be a no-op when the capsule is held.

### 5.2 Acquisition is not admission

Verification is not this crate's job (§1.1). Content is admitted because it verified against its
chain-anchored root; a tier decision MUST NOT be read as a statement that content is valid.

---

## 6. Delivery

### 6.1 Recursive discovery on an inbound miss

**A node receiving a request for a store it does not hold MUST be able to ask its own peers** rather than
answering a bare absence.

Recursion is what makes a partial peer set usable: a node's held peers are a narrow, arbitrary slice of
the network, every peer's slice differs, and an answer this node cannot give may be one hop away. A global
provider index cannot substitute — it cannot reach a holder it has never heard of, nor one reachable only
through a peer's peer. The two are **complementary**.

Requirements, each earned by a shipped implementation:

1. **Bounded by a hop budget carried in the request**, honoured on receipt as well as on send. **A hop
   that cannot read the budget MUST NOT forward** — refuse, never forward optimistically.
2. **The fan-out is an exponent, not a knob.** Reach is `fan_out ^ hop_cap`; one admitted request can
   recruit hundreds of nodes. The real per-request cost MUST be documented, and a **concurrency ceiling
   MUST NOT be described as bounding the aggregate** — it bounds how many happen at once, not how many
   happen.
3. **OFF by default.** A path spending *other* nodes' bandwidth MUST NOT be gated more loosely than one
   spending only this node's. An unrecognised configuration value MUST **fail closed**, so a typo cannot
   enable a network-wide amplifier.
4. **A disabled node MUST forward nothing**, including relaying for others — the switch cuts the chain at
   every disabled node, not only at originators.
5. **Answers from a hop are HEARSAY.** They are candidates for the **fetch** path, where verification
   makes a wrong candidate merely wasted. They MUST NOT enter an answer this node asserts to a third
   party as its own.
6. **A forwarded answer MUST NOT displace a locally-known one.** Where an answer set is capped, the cap
   MUST fall on the **forwarded** portion — otherwise one peer returning a full slate of fabricated
   holders evicts every genuine holder from the answer, for free.
7. **The requestor and this node MUST be excluded from the fan-out**, and a hop MUST NOT be asked its own
   question back.
8. **Relaying MUST draw on a budget separate from this node's own requests.** Otherwise a hop's fan-out
   is billed to the hop's allowance at its peers, and one admitted request spends a victim's budget
   across every peer it holds.

### 6.2 What recursion discloses

An implementation MUST state its **disclosure radius** — how many nodes learn of a request, none of them
chosen or enumerable by the requestor.

The absence of a requestor identity MUST NOT be described as **anonymity**: a peer is free to log what it
was asked. The disclosure happens on a **miss**, precisely when the requestor has not yet decided to
contact any holder, so it is not a disclosure a completed direct read would have made anyway.

### 6.3 Serving

A node MUST serve what it holds and advertises, and MUST NOT advertise what it cannot serve (§7.4).

---

## 7. Eviction and advertisement

### 7.1 Eviction is a retraction

**Every eviction is also an advertising retraction.** A store dropped from the cache MUST stop being
advertised as a holding; the provider record and the holdings announcement follow the cache.

A node that evicts without retracting advertises content it cannot serve, spending other nodes' dial
budget on a guaranteed miss.

### 7.2 Eviction order

Victims are chosen by `(tier rank ascending, then within-tier selection)`. A policy MUST NOT select a
pinned entry (§4.2). Returning too few victims is permitted (§4.3).

### 7.3 A recency signal driven by inbound requests is attacker-chosen

If "last access" is bumped by the same call that serves an **inbound peer request**, then on a serving
node the eviction order becomes an attacker-chosen value — a peer keeps its own content resident and lets
another's go cold.

Any recency input MUST distinguish a **local** read from an **inbound serve**, or MUST NOT be used to
order eviction.

### 7.4 Advertisement follows holdings

The advertised set MUST be derivable from what is held. An implementation MUST expose the current
holdings and the retraction set produced by each admission.

---

## 8. Trust

Every peer is untrusted. An exchange decision reads peer-supplied claims — what a peer says it holds,
wants, or will pay. **A claim is not evidence.**

### 8.1 Ranking

**An algorithm MUST NOT promote a candidate on the strength of a declaration.** It MAY demote on
evidence. For every ordering input: *can this move a candidate UP?* If a peer supplies it, the answer MUST
be no.

### 8.2 Silence

**Silence is the cheapest adversarial claim.** For every ranking input the specification MUST state what
an **absent** value does, and **an absent value MUST NOT outrank a present one**. A guard whose rationale
names a *behaviour* is walked past by a peer that declines to exhibit it.

### 8.2A Peer conduct: a claim that is not honoured

A peer that **claims to hold content and then does not deliver it** is evidence — but evidence of *what*
depends entirely on **how** it failed, and an implementation MUST NOT collapse the cases.

**This is the distinction between a malicious peer and a distressed honest one, and it is the whole
problem.** A peer that lies and a peer that is overloaded, rate-limited, half-partitioned, or mid-restart
produce the *same* observable at the transport layer: nothing arrived.

#### Three classes, only two of which are verifiable

| class | what happened | verifiable? |
|---|---|---|
| **Proven lie** | delivered bytes that fail verification against the chain-anchored root | **YES** — arithmetic, not judgement |
| **Self-contradiction** | claimed to hold it, then answered a direct request for it with an absence | **YES** — the peer contradicted its own claim |
| **Non-performance** | timeout, reset, silence, truncation, or persistent slowness | **NO** — indistinguishable from distress |

**Only the two verifiable classes MAY carry a durable penalty.** They are facts about what the peer said
and did, checkable without trusting anyone.

**Non-performance MUST NOT.** It is the signal §8.3 forbids persisting, and the reason is not caution but
a measured attack: an exclusion driven by non-delivery lets peers that withhold assigned chunks brand an
**honest** holder until only attacker-supplied candidates are ever asked for.

#### An attacker can manufacture distress in a THIRD party

This is the constraint that shapes everything else. An adversary who can degrade an honest peer — by load,
by occupying its connection slots, by partitioning it — can make that peer *look* unreliable to everyone
else. **A non-performance penalty is therefore not merely unreliable, it is weaponisable against a peer
the attacker does not control.**

Consequently:

1. **Non-performance penalties MUST decay with time**, on the same monotonic tick basis as everything else
   (§1.3), so a distressed peer recovers **without intervention and without needing to prove anything**.
2. **A non-performance penalty MUST NOT reduce a peer's dial share to zero.** A peer that can never be
   retried can never demonstrate recovery, which converts a transient penalty into the permanent exclusion
   this section exists to prevent.
3. **The penalty MUST be cheap enough that inducing it is not worth an attacker's effort.** If degrading a
   competitor is cheaper than serving content, the reputation system has become the attack surface.

#### Dial budget

Conduct history MAY order dial preference — spending fewer attempts on peers that have historically not
delivered — subject to §8.1: it MAY **demote on evidence** and MUST NOT **promote on a declaration**. A
peer's own claims about its capacity, uptime, or holdings MUST NOT raise its dial share.

**Silence MUST NOT be cheaper than answering honestly.** A peer that declines to respond MUST NOT thereby
rank better than one that answers "I do not have it" (§8.2).

#### Reputation is LOCAL and MUST NOT be gossiped as fact

Conduct records are this node's own observations. An implementation MUST NOT accept another peer's
assessment of a third party as evidence, and MUST NOT publish its own as an assertion about them.

Gossiped reputation is a defamation primitive: it lets an attacker degrade a peer everywhere at once
without ever interacting with it, and it cannot be verified by the recipient. **A peer's misconduct is
demonstrated to this node by that peer, or it is not demonstrated.**

#### Persistence

Conduct state that persists MUST follow §2A.5: it is I/O, so it enters the decision core as a
**caller-supplied input**, never read or written by the core itself. It MUST be bounded per §8.4 — it is
keyed by peer identity, which is attacker-supplied — and an unreadable record MUST fail to the
**neutral** state, never to a penalised one, or losing a file becomes an exclusion.

### 8.3 Exclusion

**A durable, cross-transfer exclusion MUST NOT rest on a signal that cannot distinguish a lie from a
transport failure.** Persisting one lets peers that withhold assigned chunks brand an honest holder until
only attacker-supplied candidates are ever asked for.

An exclusion earned by a **proven** lie — a whole-blob hash mismatch against a chain anchor — is a
different fact and MAY persist.

### 8.4 Unbounded state keyed by untrusted input

Any store, cache, or ban list keyed by peer-supplied values MUST be bounded, and its eviction policy
stated. Refusal at a limit MUST be weighed against truncation: **refusing at a limit can be a cheaper
denial than the one the limit prevents.**

### 8.5 Load, exhaustion, and denial of service

This crate sits on inbound peer traffic, so **every path a stranger can trigger is an attack surface** and
MUST be bounded before the work is done, not after.

1. **Admit before you work.** Rate limiting, budget checks and concurrency permits MUST be taken **before**
   a peer is selected, a dial is opened, or a lookup is walked. A limiter consulted after the expensive
   step bounds nothing.
2. **Meter by AUTHENTICATED identity.** The key MUST be the identity the transport verified, never one the
   caller supplies. A path that falls back to a placeholder identity collapses every requestor into **one
   shared bucket**, which is a worse denial surface than no limiter at all — one abusive caller then
   exhausts the allowance of everyone.
3. **Relayed work draws on a separate budget** from this node's own (§6.1.8), or a hop spends a victim's
   allowance across every peer it holds.
4. **No unbounded queue, map, or retry.** Everything keyed by peer-supplied values is bounded with a
   stated eviction policy (§8.4). Retries MUST be finite and MUST back off.
5. **Shed load; do not degrade silently.** Under pressure an implementation MUST refuse work and say so
   rather than accepting it and becoming slow. A silent slowdown is indistinguishable from an outage and
   cannot be diagnosed by the operator (§10).
6. **A single peer MUST NOT be able to consume the whole dial budget**, nor to fill the cache, nor to
   occupy every concurrency slot. Per-peer shares MUST exist for each.
7. **Bound the cost of a request before granting it.** A request whose work is proportional to an
   attacker-chosen number MUST have that number clamped at the boundary.

**Cache thrashing is a denial vector, not only an inefficiency.** A peer that can drive admission and
eviction in a loop spends this node's disk bandwidth indefinitely while producing no net change in what is
held. The displacement margin (§3.2) is the primary defence and MUST NOT be configurable to zero.

**The reputation system is itself an attack surface** (§8.2A): if degrading a competitor costs an attacker
less than serving content, it has become the cheapest available attack rather than a defence.

---

## 9. Configuration

Every switch MUST have a stated default and a stated failure mode. An unrecognised value MUST resolve to
the **safe** setting, never the permissive one.

| setting | default | unrecognised value |
|---|---|---|
| total disk allocation | implementation-defined, documented | reject, do not guess |
| read-triggered whole-capsule acquisition (§5.1) | **ON** | ON — it spends only this node's disk |
| recursive discovery on inbound miss (§6.1) | **OFF** | **OFF** — it spends other nodes' bandwidth |
| displacement margin (§3.2) | implementation-defined, documented | reject |

The asymmetry between the two behavioural defaults is deliberate and MUST be preserved: one is local and
bounded, the other recruits third parties.

---

## 10. Observability

An implementation MUST expose, readable by an operator without a debugger:

1. current holdings and the retraction set of the last admission (§7.4);
2. per-tier occupancy against per-tier bound (§4.2);
3. whether recursive discovery is enabled, and its disclosure radius (§6.2);
4. the reason an eviction chose its victims, sufficient to replay it offline (§1.3).

**A log line proving code ran is not an effect.** Where a decision is already made identically one layer
down, that MUST be stated rather than presented as a behaviour change.

---

## 11. Composition and versioning

### 11.1 No re-implementation

**This crate MUST NOT re-implement behaviour owned by a crate it composes.** A second implementation of a
shared behaviour is a future byte-drift bug. Where a composed crate exposes a decision seam, this crate
MUST implement it rather than build a rival.

### 11.2 A composed seam MUST be assumed lossy until read

At least one boundary in this stack silently drops a field its producer set. Composing on a seam without
reading what survives it is building on an assumption.

### 11.3 Single-version identifier types

Identifier types crossing a composed crate's public API — content and peer identifiers above all — MUST
resolve to **one** version across the dependency graph. Two majors of one type are distinct types.

---

## 11A. Public API

The sections above specify **behaviour**; this one pins the **surface** that realises it, so an
independent implementation has something concrete to be built against. Where the two ever disagree, the
behavioural clause wins and the signature is the defect.

Everything named here is exported from the crate root. Types are grouped by the module that owns them.

### 11A.1 The tier ladder — `tier`

```rust
pub enum CacheTier { Tier0Precache, Tier1Demand, Tier2Bribed }
impl CacheTier { pub const fn rank(self) -> u8; }

pub const DEFAULT_TIER: CacheTier;                 // Tier1Demand — the protected fail-safe

pub fn effective_tier(tiers: impl IntoIterator<Item = CacheTier>) -> Option<CacheTier>;

pub struct CacheEntry { pub tier: CacheTier, pub last_access_ticks: u64 }
pub fn evict_key(entry: &CacheEntry) -> (u8, u64);
```

`rank` is ascending-evicts-first, so sorting by `evict_key` IS the eviction order (§2.1). `effective_tier`
returns `None` when no source holds an opinion, which callers pair with `DEFAULT_TIER`; it is not
defaulted internally so *"nobody claimed this"* stays distinguishable from *"somebody claimed the
default"* (§2.2).

### 11A.2 Relevance — `relevance`

```rust
pub struct RelevanceWeights { pub xor: f64, pub scarcity: f64, pub demand: f64,
                              pub recency: f64, pub pin_adjacent: f64, pub pinned: f64 }
pub struct RelevanceInputs  { pub content_id: [u8; 32], pub size_bytes: u64,
                              pub known_provider_count: u32, pub local_read_count: u32,
                              pub reads_recency_ticks: Option<u64>, pub is_pinned: bool,
                              pub pin_adjacent: bool }
pub struct NodeContext      { pub peer_id: [u8; 32], pub weights: RelevanceWeights }
pub struct RelevanceValue(pub f64);

pub fn relevance(store: &RelevanceInputs, node: &NodeContext) -> RelevanceValue;
pub fn xor_proximity(content_id: &[u8; 32], peer_id: &[u8; 32]) -> f64;
pub fn in_keyspace_neighbourhood(content_id: &[u8; 32], peer_id: &[u8; 32]) -> bool;
pub const INBOUND_DEMAND_MIN_PROXIMITY: f64;       // 0.5 — the keyspace midpoint
```

`size_bytes` is carried and deliberately **not** scored (§0.1); it is the weight term selection consumes.
`reads_recency_ticks` MUST be attributed to LOCAL reads only (§7.3).

### 11A.3 Selection — `selection`

```rust
pub struct SelectionSeed(/* private */);
impl SelectionSeed {
    pub const fn from_node_local(value: u64) -> Self;
    pub fn from_peer_id(peer_id: &[u8; 32]) -> Self;
}

pub struct SelectionCandidate<Id> { pub id: Id, pub tier: CacheTier, pub size_bytes: u64,
                                    pub score: RelevanceValue, pub pinned: bool }
pub struct Selection<Id> { pub retained: Vec<Id>, pub rejected: Vec<Id> }

pub fn select_within_capacity<Id: Copy>(candidates: &[SelectionCandidate<Id>],
                                        capacity_bytes: u64,
                                        seed: SelectionSeed) -> Selection<Id>;

pub const MIN_DISPLACEMENT_MARGIN: f64;
pub struct DisplacementMargin(/* private */);
impl DisplacementMargin { pub fn new(requested: f64) -> Self; pub fn get(self) -> f64; }
pub fn may_displace(incumbent: RelevanceValue, candidate: RelevanceValue,
                    margin: DisplacementMargin) -> bool;
```

`SelectionSeed`'s field is private and both constructors name a node-local source: a peer-supplied value
cannot reach the tiebreak by accident (§4.4). `Selection::rejected` is returned in **eviction order**
(lowest tier first), so it can be handed straight to eviction without a second sort that could drift.
`DisplacementMargin` floors in its **constructor**, so no call site can hold a zero margin (§8.5).

### 11A.4 The algorithm seam — `algorithm`

```rust
pub struct StoreFacts { pub tier: CacheTier, pub score: RelevanceValue }

pub trait ExchangeAlgorithm<Id>: Send + Sync {
    fn facts(&self, id: &Id) -> Option<StoreFacts>;
}

pub struct AlgorithmSet<Id>;
impl<Id> AlgorithmSet<Id> {
    pub fn new() -> Self;
    pub fn with(self, source: Box<dyn ExchangeAlgorithm<Id>>) -> Self;
    pub fn facts(&self, id: &Id) -> Option<StoreFacts>;
    pub fn facts_or_default(&self, id: &Id) -> StoreFacts;
}
```

**This is the whole pluggable surface** (§6): one method, answering *which tier* and *how desirable within
it*. Composition is a maximum over claiming sources and is NOT itself pluggable, so registration order
cannot change policy. `None` is not a demotion — it withdraws a claim, leaving the remaining sources to
answer (§2.2).

A paid-retention algorithm (§2.3) is added as one more `ExchangeAlgorithm` returning `Tier2Bribed`, with
promotion and demotion both travelling this same composition rather than private state. No signature here
changes to admit it.

### 11A.5 Eviction — `eviction`

```rust
pub struct TieredPolicy;
impl TieredPolicy {
    pub fn new(algorithms: Arc<AlgorithmSet<CapsuleIdentity>>, seed: SelectionSeed) -> Self;
}
impl dig_store_cache::EvictionPolicy for TieredPolicy { /* select_evictions */ }
```

An implementation of `dig-store-cache`'s **existing** seam, never a rival (§11.1). It deliberately does
not read `EvictionEntry::last_access`, which that crate bumps in `get()` — the same call the serving path
makes for an inbound peer request, making it attacker-chosen on a serving node (§7.3).

### 11A.6 Acquisition — `acquisition`

```rust
pub struct BackfillPolicy { pub enabled: bool }        // Default: enabled
pub enum AcquisitionDecision { Acquire, SkipDisabled, SkipAlreadyHeld, SkipInFlight }

pub fn decide(policy: BackfillPolicy, capsule: &CapsuleIdentity, already_held: bool,
              in_flight: &HashSet<CapsuleIdentity>) -> AcquisitionDecision;
```

Dedup is keyed on the **capsule** — `(store_id, root_hash)` — not the store, so a newer generation is
still acquired (§5.1). The caller performs the pull and MUST NOT block the triggering read on it.

### 11A.7 Holdings — `holdings`

```rust
pub struct HoldingsDelta { pub announce: Vec<CapsuleIdentity>, pub retract: Vec<CapsuleIdentity> }
impl HoldingsDelta { pub fn is_empty(&self) -> bool; }

pub fn after_admission(admitted: CapsuleIdentity, evicted: &[CapsuleIdentity]) -> HoldingsDelta;
pub fn after_eviction(evicted: &[CapsuleIdentity]) -> HoldingsDelta;
pub fn reconcile(advertised: &[CapsuleIdentity], held: &[CapsuleIdentity]) -> HoldingsDelta;
```

`after_admission` takes `dig-store-cache`'s `Admission.evicted` directly. `reconcile` is the repair path
that makes a missed retraction recoverable rather than permanent (§7.1).

### 11A.8 Reward accounting — `reward`

```rust
pub struct ClaimId(pub [u8; 32]);                      // derived from the chain event
pub struct RewardClaim { pub claim_id: ClaimId, pub store: CapsuleIdentity, pub amount: u64 }
pub enum RecordOutcome { Recorded, AlreadyRecorded }

pub struct RewardLedger;
impl RewardLedger {
    pub fn empty() -> Self;
    pub fn from_chain_claims(claims: impl IntoIterator<Item = RewardClaim>) -> Self;
    pub fn record(&mut self, claim: RewardClaim) -> RecordOutcome;
    pub fn claimed_for(&self, store: &CapsuleIdentity) -> u64;
    pub fn reconcile_from_chain(&mut self, claims: impl IntoIterator<Item = RewardClaim>);
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
}
```

Idempotence rests entirely on `ClaimId` being **chain-derived**; a locally-minted id differs on every
retry and double-counts (§2A.4). `reconcile_from_chain` REPLACES rather than merges, because a merge
preserves the uncorroborated entry reconciliation exists to remove (§2A.2/§2A.3). Persistence and
durability are the caller's; the ledger reaches the decision core as an input (§2A.5).

### 11A.9 Recursive discovery — `discovery`

```rust
pub enum Provenance { FirstHand, Hearsay }

pub struct RecursionConfig { pub enabled: bool, pub fan_out: u8, pub hop_cap: u8,
                             pub max_hearsay_answers: usize }
impl RecursionConfig { pub fn worst_case_nodes_recruited(&self) -> u64; }   // fan_out ^ hop_cap
impl Default for RecursionConfig { /* enabled: false */ }

pub fn parse_enabled(raw: Option<&str>) -> bool;                            // fails closed

pub struct InboundAsk<Peer> { pub requestor: Peer, pub hops_remaining: Option<u8> }
pub enum ForwardRefusal { Disabled, HopBudgetSpent, UnreadableHopBudget,
                          NoEligiblePeers, RelayBudgetSpent }
pub enum ForwardDecision<Peer> { Forward { peers: Vec<Peer>, hops_remaining: u8 },
                                 Refuse(ForwardRefusal) }

pub fn decide_forward<Peer: Copy + PartialEq>(config: &RecursionConfig, ask: &InboundAsk<Peer>,
                                              this_node: &Peer, known_peers: &[Peer],
                                              relay_budget_available: bool) -> ForwardDecision<Peer>;

pub fn merge_answers<Answer: Copy>(config: &RecursionConfig, first_hand: &[Answer],
                                   hearsay: &[Answer]) -> Vec<(Answer, Provenance)>;
```

`hops_remaining: Option<u8>` makes an unreadable budget representable, which is what lets §6.1.1's refusal
be expressed at all. `worst_case_nodes_recruited` is also the **disclosure radius** (§6.2).
`max_hearsay_answers` caps only the forwarded portion, so a flood cannot evict a first-hand answer
(§6.1.6).

### 11A.10 Peer conduct — `conduct`

```rust
pub enum ConductEvidence { ProvenLie, SelfContradiction, NonPerformance, HonestAnswer }
impl ConductEvidence { pub const fn is_verifiable(self) -> bool; }

pub struct ConductRecord { pub proven_faults: u32, pub non_performance: u32,
                           pub last_update_ticks: u64 }
impl ConductRecord { pub fn neutral() -> Self; }

pub const NON_PERFORMANCE_PENALTY: u32;
pub const NON_PERFORMANCE_CEILING: u32;
pub const NON_PERFORMANCE_DECAY_TICKS: u64;
pub const MIN_NON_PERFORMANCE_DIAL_SHARE: f64;         // > 0, so recovery stays demonstrable

pub fn observe(record: ConductRecord, evidence: ConductEvidence, now_ticks: u64) -> ConductRecord;
pub fn decay(record: ConductRecord, now_ticks: u64) -> ConductRecord;
pub fn dial_share(record: ConductRecord) -> f64;
```

`ConductEvidence` is this crate's **own local, peer-scoped** type. It records what a peer did to this node
and carries no on-chain authority; it MUST NOT be replaced by an L2 evidence type to borrow one.

There is deliberately **no** constructor taking another peer's assessment, and no serialisation of one:
reputation is local, and gossiped reputation is a defamation primitive (§8.2A). `dial_share` only ever
demotes — no input raises a peer's share (§8.1).

### 11A.11 Load admission — `admission`

```rust
pub struct AuthenticatedPeer(/* private */);
impl AuthenticatedPeer { pub const fn from_verified_session(peer_id: [u8; 32]) -> Self; }

pub enum WorkKind { Own, Relayed }
pub enum Refusal { GlobalCeiling, PeerShare, RelayBudget, MeterFull, RequestTooLarge }

pub struct AdmissionLimits { pub global_ceiling: u32, pub per_peer_share: u32,
                             pub relay_ceiling: u32, pub max_tracked_peers: usize,
                             pub max_request_units: u32 }

pub struct AdmissionMeter;
impl AdmissionMeter {
    pub fn new(limits: AdmissionLimits) -> Self;
    pub fn admit(&mut self, peer: AuthenticatedPeer, kind: WorkKind,
                 requested_units: u32) -> Result<(), Refusal>;
    pub fn release(&mut self, peer: AuthenticatedPeer, kind: WorkKind);
    pub fn in_flight_total(&self) -> u32;
}
```

`AuthenticatedPeer`'s field is private and its only constructor names what it asserts, so the
shared-placeholder-bucket failure of §8.5.2 is **unrepresentable** rather than merely discouraged.
`admit` is called BEFORE the work, and clamps `requested_units` at the boundary (§8.5.1, §8.5.7).

### 11A.12 Error taxonomy

This crate returns **no** `Error` type, and that is deliberate: it decides, it does not perform. Every
failure it can express is a *decision outcome* with a named, exhaustive, stable set of reasons —
`AcquisitionDecision`, `ForwardRefusal`, `Refusal`, `RecordOutcome` — and each variant states the ONE
condition that produced it, so an operator can tell which bound was reached (§10).

I/O errors belong to the crates that perform the I/O: `dig-store-cache::CacheError` for admission
mechanics, `dig-download` for byte movement, `dig-dht` for announce and retract. This crate MUST NOT wrap
or restate them.

### 11A.13 What is NOT in the surface, and why

- **No `Tier2Bribed` producer.** The paid tier is carried end to end and nothing here populates it; the
  algorithm is deferred (§2.3).
- **No price, payer, or settlement type.** Those belong to that algorithm, whose accounting unit is
  unspecified (§0.2).
- **No verification.** Content is accepted because it verifies against its chain anchor, elsewhere (§5.2).
- **No clock, no RNG, no filesystem, no socket.** Ticks, seeds and persisted state are all inputs (§1.3).
- **No identifier type of its own.** `CapsuleIdentity` is re-exported from `dig-store-cache`, so the graph
  holds one version (§11.3).

---

## 12. Conformance

An implementation conforms when:

1. paid retention is never sacrificed to hold unpaid content, and no exchange rate between the objectives
   exists (§0);
2. size does not enter the relevance score (§0.1);
3. cross-tier precedence is absolute and no score moves a store between tiers (§2.1);
4. effective tier is the maximum across enumerated sources, and a promotion survives a lower reason
   lapsing (§2.2);
5. an unreadable tier tag resolves to the protected default (§2.3);
6. the seam admits a paid-retention algorithm meeting all four requirements of §2.4 without signature
   changes;
6a. rewards claimed are recorded per store, persistently and durably, reconcilable against chain state,
    idempotent on replay, biased toward under-counting, and supplied to the decision core as an input
    rather than read by it (§2A);
7. every peer-influenced score input is bounded and clamped (§3.1);
8. displacement requires a margin (§3.2);
9. within-tier selection maximises the COUNT of stores against residual capacity and is not
   sort-by-score-and-fill (§4.1);
10. pinned entries are never evicted and may exceed the allocation (§4.2);
11. ties on profit and size break randomly from a node-local, non-peer-derivable seed supplied as an input
    (§4.4);
12. a remotely-satisfied read triggers a deduplicated, non-blocking whole-capsule acquisition (§5.1);
13. an inbound miss can recurse under a carried hop budget, defaults OFF, fails closed on an unrecognised
    setting, forwards nothing when disabled, and never lets a forwarded answer displace a locally-known
    one (§6.1);
14. the disclosure radius is stated and never described as anonymity (§6.2);
15. every eviction retracts the corresponding advertisement (§7.1);
16. no recency signal used for eviction is drivable by inbound requests (§7.3);
17. no ordering input a peer supplies can promote a candidate, and every absent value is specified
    (§8.1, §8.2);
17a. a peer's failure to honour a claim is classified as proven lie, self-contradiction, or
     non-performance; only the first two carry durable penalties; non-performance decays on a tick basis,
     never reduces a dial share to zero, and is cheap enough that inducing it is not worth an attacker's
     effort (§8.2A);
17b. reputation is local: no third party's assessment is accepted as evidence and none is published as an
     assertion (§8.2A);
18. no durable exclusion rests on a signal that cannot distinguish a lie from a transport failure (§8.3);
19. all state keyed by untrusted input is bounded (§8.4);
19a. every peer-triggered path is admitted before the work is done, metered by AUTHENTICATED identity
     rather than a caller-supplied or placeholder one, and no single peer can consume the whole dial
     budget, cache, or concurrency (§8.5);
19b. the displacement margin cannot be configured to zero (§8.5);
20. every configuration switch has a stated default and fails to the safe setting (§9);
21. the four observability surfaces of §10 exist;
22. no behaviour owned by a composed crate is re-implemented, and identifier types resolve to one version
    (§11);
23. the decision core is pure and every decision is replayable offline from recorded inputs (§1.3).
