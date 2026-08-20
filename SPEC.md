# dig-sex — normative specification

**Status: MODEL + CONSTRAINTS.** This describes the store-exchange model as it exists in the ecosystem
today and is being folded into this crate. Behavioural clauses land with the implementation; the model
and the constraints below are normative now.

## 1. Scope

`dig-sex` (Store EXchange) is the **policy layer** for exchanging DIG stores between peers, and the home
of the **cache/relevance/tier system** that decides what a node holds.

It answers: *which* store to acquire, *with whom* to exchange, *what to keep*, *what to sacrifice first*,
and *when a fresh candidate is worth displacing an incumbent*.

It is **not** a transport, a discovery mechanism, or a fetcher, and MUST NOT become one. `dig-dht` finds
providers, `dig-pex` exchanges peer records, `dig-download` moves bytes, `dig-store-cache` performs the
on-disk admission and eviction **mechanics**. This crate owns the **decisions** those mechanisms carry
out.

**The dividing line is mechanism versus policy.** *"Stage to a temp file, fsync, rename"* is mechanism.
*"Which store, from whom, and what do I drop to make room"* is policy.

## 2. The tier model

Every cacheable store holds a **tier**. Three exist:

| tier | earned by | eviction precedence |
|---|---|---|
| **`Tier0Precache`** | speculative acquisition (DHT-neighbourhood precache) | sacrificed **FIRST** |
| **`Tier1Demand`** | a real read — local, or an inbound peer request | sacrificed only after all `Tier0` |
| **`Tier2Bribed`** | a backer paid to keep it resident | sacrificed **LAST** |

### 2.1 Cross-tier precedence is absolute; score orders only within a tier

**A relevance score MUST NOT move a store across tiers.** Across tiers, eviction precedence is fixed by
the tier alone. Within a tier, the score orders candidates.

This is what makes the system a **capacity ladder**: higher tiers claim disk first, and **lower tiers
occupy only the space higher tiers did not**. A `Tier0` entry is not "less relevant" than a `Tier1`
entry — it is *sacrificeable first regardless of relevance*, and that is deliberate.

### 2.2 A store's tier is the MAXIMUM across its sources

A store may earn a tier by more than one route simultaneously. **Its effective tier is the maximum**, so
acquiring a store speculatively and then reading it promotes it; the promotion MUST NOT be lost when the
speculative reason lapses.

### 2.3 `Tier2Bribed` exists; its algorithm does not yet

The paid tier is part of the model **now**. The algorithm that decides who pays, how much, and what
proves it is **deferred** and MUST NOT be invented here.

The seam MUST admit that algorithm later **without signature changes** — which means an implementation
must be able to read what it needs (a price, a payer, a settlement outcome) and to demote a non-payer
through the **same** evidence channel every other tier uses. An algorithm forced to keep non-payment in
private state has been given an interface that does not fit it.

## 3. Relevance scoring

Within a tier, a store's desirability is a **score**, and the model is deliberately bounded:

- **The primary signal is XOR distance** between the content id and this node's peer id. Content landing
  near this node in the 256-bit keyspace is content the node is naturally responsible for.
- Around it sit **bounded, weighted bonuses**: replication scarcity (keep what few others hold), local
  demand (keep what our own users read), pin adjacency, and a large **pinned** bonus.

**Scoring MUST remain pure and deterministic** — no clock, no network, no I/O. Time enters only as
caller-supplied tick counters, so the same inputs always yield the same decision and any eviction can be
**replayed and audited offline**. That property is load-bearing: an exchange-policy regression is
otherwise invisible, because content still arrives, just slower and from worse peers.

**A pinned entry MUST NOT be evicted**, and a pin MAY push a node over its configured capacity. That is
the operator's explicit override.

## 4. Acquisition

### 4.1 A read creates relevance

When a read for a `(store_id, root)` is satisfied **from another node**, the node SHOULD acquire the
**whole** `.dig` capsule for that generation in the background, so the next read is served locally. A
one-off remote read becomes a durable local copy **without** the store being subscribed.

This is `Tier1Demand`: the request itself is the evidence of relevance. It MUST be non-blocking, MUST NOT
delay the read that triggered it, and MUST deduplicate concurrent triggers for the same `store:root` into
one acquisition.

### 4.2 Acquisition is not admission

Verification is **not** this crate's job and MUST NOT be re-implemented here: content is accepted because
it verifies against its chain-anchored root, and the cache is caller-verifies by contract. A tier decision
MUST NOT be read as a statement that content is valid.

## 5. Eviction is a retract

**Every eviction is also an advertising retraction.** A store dropped from the cache is one the node MUST
stop advertising as a holding — the provider record and the holdings announcement follow the cache, not
the other way round.

A node that evicts without retracting advertises content it cannot serve, which spends other nodes' dial
budget on a guaranteed miss.

## 6. Pluggability

What plugs in is **how a candidate earns a tier** and **how it scores within one**. The tier ladder and
its precedence are the fixed frame.

Several relevance strategies are valid simultaneously and the architecture MUST support that — this is a
set of tiered acquisition sources competing for one capacity budget, not a single algorithm with a single
policy.

**Where a composed crate already exposes a decision seam, this crate MUST implement it rather than build a
rival.** `dig-store-cache`'s eviction-policy trait is such a seam, and its own documentation describes the
relevance model as the brain it *"will later consult"* — the two were designed to meet.

## 7. Trust

Every peer is untrusted (NC-12). An exchange decision reads peer-supplied claims — what a peer says it
holds, wants, or will pay. **A claim is not evidence.**

### 7.1 Ranking

**An algorithm MUST NOT promote a candidate on the strength of a declaration.** It MAY demote on
evidence. For every ordering input the question is *"can this move a candidate UP?"* — and if a peer
supplies it, the answer must be no.

**Silence is the cheapest adversarial claim.** For every ranking input the specification MUST state what
an **absent** value does, and **an absent value MUST NOT outrank a present one**. A guard whose rationale
names a behaviour is walked past by a peer that declines to exhibit it.

### 7.2 A recency signal driven by inbound requests is attacker-chosen

If "last access" is bumped by the same call that serves an inbound peer request, then on a serving node
**the eviction order is an attacker-chosen value** — a peer can keep its own content resident and let
another's go cold. Any recency input MUST distinguish a local read from an inbound serve, or MUST NOT be
used to order eviction.

### 7.3 Exclusion

**A durable, cross-transfer exclusion MUST NOT rest on a signal that cannot distinguish a lie from a
transport failure.** Persisting one lets peers that withhold their assigned chunks brand an honest holder
until only attacker-supplied candidates are ever asked for. An exclusion earned by a **proven** lie — a
whole-blob hash mismatch against a chain anchor — is a different fact and MAY persist.

## 8. Composition

**This crate MUST NOT re-implement behaviour owned by a crate it composes.** A second implementation of a
shared behaviour is a future byte-drift bug.

**A composed seam MUST be assumed lossy until read.** At least one boundary in this stack silently drops a
field its producer set; composing on top of a seam without reading what survives it is building on an
assumption.

## 9. Versioning

Identifier types crossing a composed crate's public API — content and peer identifiers in particular —
MUST resolve to **one** version across the dependency graph. Two majors of the same type are distinct
types.

## 10. Honesty of effect

A change here MUST have an effect an operator can observe, and this document MUST say what it is. **A log
line proving the code ran is not an effect.** Where a decision is already made identically one layer down,
that MUST be stated rather than presented as a behaviour change.

## 11. Conformance

An implementation conforms when:

1. cross-tier precedence is absolute and no score moves a store between tiers (§2.1);
2. a store's effective tier is the maximum across its sources (§2.2);
3. `Tier2Bribed` is expressible, and a paid-retention algorithm could be added without changing
   signatures (§2.3);
4. scoring is pure and any eviction is replayable offline (§3);
5. a read satisfied remotely triggers a deduplicated, non-blocking whole-capsule acquisition (§4.1);
6. every eviction retracts the corresponding advertisement (§5);
7. no ordering input a peer supplies can promote a candidate, every absent value is specified, and no
   recency signal is drivable by inbound requests (§7);
8. no durable exclusion rests on a signal that cannot distinguish a lie from a transport failure (§7.3);
9. no behaviour owned by a composed crate is re-implemented (§8);
10. identifier types resolve to one version (§9);
11. every claimed effect is observable and every non-effect disclosed (§10).
