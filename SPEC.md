# dig-sex — normative specification

**Status: SCOPE + CONSTRAINTS.** The behavioural clauses arrive with the implementation. What is
normative today is what this crate is for, what it may not do, and the constraints any exchange
algorithm must satisfy. Those constraints are not aspirational: each one is derived from a defect this
ecosystem has already measured, and several were produced by an adversarial review of this crate's own
first design.

## 1. Scope

`dig-sex` (Store EXchange) is the **policy layer** for exchanging DIG stores between peers. It answers
*which* store to exchange, *with whom*, *in what order*, and *why*.

It is **not** a transport, a discovery mechanism, or a fetcher, and it MUST NOT become one. `dig-dht`
finds providers, `dig-pex` exchanges peer records, `dig-download` moves bytes, `dig-store-cache` holds
what arrived. This crate **composes** them behind one decision surface.

**The dividing line is mechanism versus policy.** *"How do I fetch a range"* is mechanism and stays where
it is. *"Which store should I fetch, from whom, and in what order"* is policy and belongs here.

## 2. The algorithm seam is the primary deliverable

Exchange algorithms are **pluggable**, and that is what this crate exists for. At least one intended
implementation is **incentivised** exchange, where peers trade on terms rather than on relevance alone.

**A design that can express only the relevance algorithm has failed this specification**, regardless of
how well it expresses it. The seam MUST be validated against the incentivised algorithm — a design is
conformant only if an incentivised implementation can be written against it **without changing its
signatures**.

### 2.1 What an algorithm MUST be able to see

An algorithm that cannot see enough is useless; one that sees peer claims as facts will rank on
attacker-chosen values. The interface MUST therefore distinguish **verified fact** from **attributed
claim**, and MUST NOT make a claim unreadable in the name of safety — an algorithm that cannot read a
price cannot price.

Concretely, the seam MUST admit at minimum:

1. a **price**, readable by the algorithm, with a producer that can actually obtain one;
2. **settlement failure as evidence**, travelling the **same channel** as every other demotion — an
   algorithm forced to keep non-payment in private state has been given an interface that does not fit it;
3. a **money budget**, distinct from a byte budget;
4. **stake as an input**, not as something the algorithm asserts on the way out;
5. **bytes served TO a counterparty**, not only bytes fetched from them, or reciprocity is inexpressible.

## 3. Trust

Every peer is untrusted (NC-12). An exchange decision reads peer-supplied claims — what a peer says it
holds, what it says it wants, what it says it charges. **A claim is not evidence.** Content is accepted
because it verifies against its on-chain-anchored root, never because a peer offered it.

### 3.1 Ranking

**An algorithm MUST NOT promote a candidate on the strength of a declaration.** It MAY demote on
evidence. The asymmetry is the rule: for every ordering input, the question is *"can this move a candidate
UP?"* — and if a peer can supply it, the answer must be no.

**Silence is the cheapest adversarial claim.** For every ranking input, the specification MUST state what
an **absent** value does, and **an absent value MUST NOT outrank a present one**. A guard whose rationale
names a *behaviour* is walked past by a peer that declines to exhibit it.

### 3.2 Exclusion

**A durable, cross-transfer exclusion MUST NOT be driven by a signal that cannot distinguish a lie from a
transport failure.** `dig-download` deliberately refuses to persist such a signal, because doing so lets
peers that withhold their assigned chunks brand an honest holder until only attacker-supplied candidates
are ever asked for. An exclusion earned by a **proven** lie — a whole-blob hash mismatch against a chain
anchor — is a different fact and MAY persist.

## 4. Composition

**This crate MUST NOT re-implement any behaviour owned by a crate it composes.** A second implementation
of a shared behaviour is a future byte-drift bug; this ecosystem carries fourteen rival implementations of
one parser because that rule was not applied.

Where a composed crate already exposes a decision seam, `dig-sex` MUST implement it rather than replace
it — `dig-store-cache`'s eviction policy is such a seam.

**A composed seam MUST be assumed lossy until read.** At least one boundary in this stack silently drops
a field its producer set. A design that composes on top of a seam without reading what survives it is
building on an assumption.

## 5. Versioning

Types crossing a composed crate's public API — content and peer identifiers in particular — MUST resolve
to **one** version across the dependency graph. Two majors of the same type are distinct types, and the
failure is a compile error at best and a silently divided namespace at worst.

## 6. Honesty of effect

A change to this crate MUST have an effect an operator can observe, and the specification MUST say what
that effect is. **A log line proving the code ran is not an effect.**

Where a decision this crate makes is already made identically one layer down, the specification MUST say
so rather than presenting the arrangement as a behaviour change.

## 7. Conformance

An implementation conforms when:

1. an incentivised algorithm can be written against its seam without signature changes (§2);
2. no ordering input a peer supplies can promote a candidate, and every absent value is specified (§3.1);
3. no durable exclusion rests on a signal that cannot distinguish a lie from a transport failure (§3.2);
4. no behaviour owned by a composed crate is re-implemented (§4);
5. identifier types crossing composed public APIs resolve to one version (§5);
6. every claimed effect is observable, and every non-effect is disclosed (§6).
