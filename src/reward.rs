//! Reward accounting — the ledger the deferred claim mechanism will write to (SPEC §2A).
//!
//! `Tier2Bribed` is earned by payment, so this crate keeps a per-store record of rewards **claimed**
//! from an on-chain reward distributor. How a claim is constructed, submitted and proven is
//! **deferred** and is deliberately not invented here; this module specifies the store it lands in,
//! so the mechanism can be added without redesigning the accounting underneath it.
//!
//! # The chain is authoritative; this is a local view
//!
//! The ledger is a **cache of an on-chain fact**, never the fact itself. Where the two disagree the
//! chain wins, which is why [`RewardLedger::reconcile_from_chain`] exists and why every entry is keyed
//! by something the chain produced. A local record that cannot be checked against its source is not an
//! accounting record.
//!
//! # It fails toward UNDER-counting, and the asymmetry is the whole design
//!
//! The two error directions are not symmetric:
//!
//! - **Under-counting** — claimed on chain, not recorded here — costs the operator *visibility* of
//!   income they already hold. Reconciliation recovers it.
//! - **Over-counting** — recorded but never claimed, or counted twice — makes this node **hold unpaid
//!   content as though it were paid**, sacrificing genuinely paid content to do so. That violates the
//!   primary objective directly, and nothing corrects it.
//!
//! So every ambiguous case resolves downward, and [`ClaimId`] makes the commonest over-count —
//! recording the same claim twice on a retry — structurally impossible rather than merely discouraged.
//!
//! # Purity: this is an INPUT, never a read
//!
//! The decision core does not read or write this ledger. Persistence and durability are the caller's,
//! and claimed-reward figures reach the core as caller-supplied values exactly as tick counters and
//! the tie-break seed do. That keeps every tier decision replayable offline and keeps the accounting
//! testable without a chain.
//!
//! # What this does NOT settle
//!
//! Recording *what was claimed* is not deciding *what a store is worth keeping for*. These figures are
//! not a price, not a bid, and not a promise of future payment, and MUST NOT be read as any of those
//! until a specification says so.

use std::collections::HashMap;

use dig_store_cache::CapsuleIdentity;

/// The identity of one claim, derived from the chain event that produced it.
///
/// **This is what makes recording idempotent.** A claim id derived locally — a counter, a timestamp, a
/// random value — differs on every retry, so a retried submission records twice and inflates profit.
/// That is the over-counting failure arriving through the front door, so the id must come from the
/// chain event itself (a coin id, a spend id) and nothing here ever mints one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClaimId(pub [u8; 32]);

/// A reward claimed from the on-chain distributor for one store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RewardClaim {
    /// The chain-derived identity of the claim.
    pub claim_id: ClaimId,
    /// Which store the reward was earned for.
    pub store: CapsuleIdentity,
    /// The claimed amount, in the distributor's smallest unit.
    pub amount: u64,
}

/// The outcome of recording a claim — reported rather than silently absorbed, so an operator can tell
/// a genuine claim from a replay (SPEC §10: an effect must be observable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordOutcome {
    /// A claim not previously seen; the total moved.
    Recorded,
    /// This exact chain event was already recorded. The total did NOT move.
    AlreadyRecorded,
}

/// A per-store record of rewards claimed. Pure and in-memory: the caller owns persistence and
/// durability (SPEC §2A.1), and hands the resulting view to the decision core as an input.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RewardLedger {
    seen: HashMap<ClaimId, RewardClaim>,
}

impl RewardLedger {
    /// An empty ledger — and the state an unreadable persisted ledger MUST fall back to.
    ///
    /// Falling back to empty is the under-counting direction: the node forgets income it holds, which
    /// reconciliation repairs. Falling back to any remembered total would be a guess in the
    /// over-counting direction, which nothing repairs.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Rebuild from a set of claims known to have occurred on chain — the observed events keyed by
    /// their own ids, so duplicates in the input collapse rather than accumulate.
    #[must_use]
    pub fn from_chain_claims(claims: impl IntoIterator<Item = RewardClaim>) -> Self {
        let mut ledger = Self::empty();
        for claim in claims {
            ledger.record(claim);
        }
        ledger
    }

    /// Record a claim. Idempotent by [`ClaimId`]: replaying, retrying or re-observing the same chain
    /// event cannot move the total.
    pub fn record(&mut self, claim: RewardClaim) -> RecordOutcome {
        if self.seen.contains_key(&claim.claim_id) {
            return RecordOutcome::AlreadyRecorded;
        }
        self.seen.insert(claim.claim_id, claim);
        RecordOutcome::Recorded
    }

    /// Total claimed for one store. `0` for a store with no claims — the honest answer, and the
    /// under-counting direction.
    #[must_use]
    pub fn claimed_for(&self, store: &CapsuleIdentity) -> u64 {
        self.seen
            .values()
            .filter(|claim| claim.store == *store)
            .fold(0u64, |total, claim| total.saturating_add(claim.amount))
    }

    /// Replace this local view with what the chain says (SPEC §2A.2).
    ///
    /// It is a REPLACEMENT, not a merge. Merging would preserve any local entry the chain does not
    /// corroborate, which is precisely the over-counted entry reconciliation exists to remove — so a
    /// merge would make the ledger unable to correct its own worst failure.
    pub fn reconcile_from_chain(&mut self, claims: impl IntoIterator<Item = RewardClaim>) {
        *self = Self::from_chain_claims(claims);
    }

    /// How many distinct chain events are recorded. Bounded state observability (SPEC §8.4).
    #[must_use]
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    /// Whether anything is recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(byte: u8) -> CapsuleIdentity {
        CapsuleIdentity {
            store_id: [byte; 32].into(),
            root_hash: [byte; 32].into(),
        }
    }

    fn claim(id: u8, store_byte: u8, amount: u64) -> RewardClaim {
        RewardClaim {
            claim_id: ClaimId([id; 32]),
            store: store(store_byte),
            amount,
        }
    }

    /// SPEC §2A.4 — the load-bearing property. A retried submission of the SAME chain event must not
    /// move the total. The fixture uses two claims with the same id but DIFFERENT amounts, so an
    /// implementation that deduplicates on the whole record rather than on the chain-derived id still
    /// fails: a retry that reports a different amount is exactly how a real retry differs.
    #[test]
    fn replaying_a_chain_event_cannot_inflate_the_total() {
        let mut ledger = RewardLedger::empty();
        assert_eq!(ledger.record(claim(1, 7, 100)), RecordOutcome::Recorded);
        assert_eq!(
            ledger.record(claim(1, 7, 999)),
            RecordOutcome::AlreadyRecorded
        );

        assert_eq!(ledger.claimed_for(&store(7)), 100);
        assert_eq!(ledger.len(), 1);
    }

    /// Two genuinely different chain events for the same store DO accumulate — the control for the
    /// test above. Without it, a ledger that recorded nothing at all would pass the idempotence test.
    #[test]
    fn distinct_chain_events_accumulate() {
        let mut ledger = RewardLedger::empty();
        ledger.record(claim(1, 7, 100));
        ledger.record(claim(2, 7, 50));
        assert_eq!(ledger.claimed_for(&store(7)), 150);
    }

    /// Claims are attributed per store, so one store's income can never be read as another's.
    #[test]
    fn claims_are_attributed_to_their_own_store() {
        let mut ledger = RewardLedger::empty();
        ledger.record(claim(1, 7, 100));
        ledger.record(claim(2, 8, 400));

        assert_eq!(ledger.claimed_for(&store(7)), 100);
        assert_eq!(ledger.claimed_for(&store(8)), 400);
        assert_eq!(ledger.claimed_for(&store(9)), 0);
    }

    /// SPEC §2A.2/§2A.3 — reconciliation must be able to REMOVE a local entry the chain does not
    /// corroborate, because that entry is the over-count. A merge-based reconcile preserves it, which
    /// is why the fixture starts from a phantom claim rather than from an empty ledger.
    #[test]
    fn reconciliation_removes_a_local_entry_the_chain_does_not_corroborate() {
        let mut ledger = RewardLedger::empty();
        ledger.record(claim(9, 7, 1_000_000)); // never actually happened on chain

        ledger.reconcile_from_chain([claim(1, 7, 100)]);

        assert_eq!(
            ledger.claimed_for(&store(7)),
            100,
            "the phantom claim must be gone, not merged"
        );
        assert_eq!(ledger.len(), 1);
    }

    /// Reconciliation also restores an under-count — the recoverable direction. Paired with the test
    /// above so both directions are pinned; a reconcile that only ever added would pass one and fail
    /// the other.
    #[test]
    fn reconciliation_restores_an_under_count() {
        let mut ledger = RewardLedger::empty();
        ledger.reconcile_from_chain([claim(1, 7, 100), claim(2, 7, 50)]);
        assert_eq!(ledger.claimed_for(&store(7)), 150);
    }

    /// SPEC §2A.3: the fallback for a lost ledger is EMPTY (under-count), never a remembered total.
    #[test]
    fn a_lost_ledger_falls_back_to_zero_not_to_a_guess() {
        assert!(RewardLedger::empty().is_empty());
        assert_eq!(RewardLedger::empty().claimed_for(&store(7)), 0);
    }

    /// Duplicates in a chain observation collapse rather than accumulate — re-observing the same
    /// events during a rebuild is normal and must not inflate anything.
    #[test]
    fn rebuilding_from_repeated_observations_collapses_them() {
        let ledger = RewardLedger::from_chain_claims([claim(1, 7, 100), claim(1, 7, 100)]);
        assert_eq!(ledger.claimed_for(&store(7)), 100);
    }
}
