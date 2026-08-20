//! Load admission — bounding inbound work before it is done (SPEC §8.5).
//!
//! This crate sits on inbound peer traffic, so **every path a stranger can trigger is an attack
//! surface**. The rule that shapes this module is stated as an ordering, not as a feature: *admit
//! before you work*. A limiter consulted after the peer has been selected, the dial opened, or the
//! lookup walked bounds nothing — the cost has already been paid.
//!
//! # Metering by AUTHENTICATED identity, and why a placeholder is worse than nothing
//!
//! The meter key is [`AuthenticatedPeer`], a type only the transport layer can mint from an identity
//! it actually verified. A caller-supplied identity lets one abuser spend under many names; a
//! **placeholder** identity is worse still, because it collapses every requestor into ONE shared
//! bucket and lets a single abusive caller exhaust the allowance of everyone. That is a worse denial
//! surface than having no limiter at all, so this module makes it unrepresentable rather than
//! discouraged: there is no way to construct the key without asserting authentication.
//!
//! # Three separate budgets, and the separation is load-bearing
//!
//! - a **global** ceiling, so the node as a whole sheds load rather than degrading;
//! - a **per-peer** share, so no single peer can consume the whole allowance;
//! - a **relay** budget for work done on OTHERS' behalf, kept apart from this node's own — otherwise
//!   a hop's fan-out is billed to the hop's allowance at its peers, and one admitted request spends a
//!   victim's budget across every peer it holds (SPEC §6.1.8).
//!
//! # Shedding is explicit
//!
//! Refusal returns a named [`Refusal`], never a silent slowdown. A silent slowdown is
//! indistinguishable from an outage and cannot be diagnosed by the operator.
//!
//! # Bounded state
//!
//! The per-peer table is keyed by an attacker-supplied identity, so it is capped and its eviction
//! policy is stated: at the cap, an unknown peer is REFUSED rather than admitted, and refusal is the
//! cheaper failure here — admitting would mean either unbounded memory or evicting an established
//! peer's accounting, which is itself a way to launder past usage.

use std::collections::HashMap;

/// A peer identity the TRANSPORT verified.
///
/// The inner value is private and the only constructor names what it asserts, so a caller-supplied or
/// placeholder identity cannot reach the meter by accident. This is SPEC §8.5.2 enforced by the type
/// system rather than by review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AuthenticatedPeer([u8; 32]);

impl AuthenticatedPeer {
    /// Wrap an identity the transport layer authenticated. The caller asserts, by calling this, that
    /// `peer_id` came from a verified session and not from the request body.
    #[must_use]
    pub const fn from_verified_session(peer_id: [u8; 32]) -> Self {
        Self(peer_id)
    }
}

/// The kind of work being admitted — the two draw on separate budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkKind {
    /// Work this node does for itself.
    Own,
    /// Work done on another node's behalf (a relayed ask). Separately budgeted (SPEC §6.1.8).
    Relayed,
}

/// Why work was refused. Named so the operator can tell shed load from an outage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The node-wide concurrency ceiling is reached.
    GlobalCeiling,
    /// This peer already holds its share.
    PeerShare,
    /// The separate relay allowance is exhausted.
    RelayBudget,
    /// The per-peer table is full and this peer is not in it.
    MeterFull,
    /// The request asks for more work than the boundary permits (SPEC §8.5.7).
    RequestTooLarge,
}

/// The limits this node admits under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmissionLimits {
    /// Node-wide concurrent work items.
    pub global_ceiling: u32,
    /// Concurrent work items any ONE peer may hold. Strictly below `global_ceiling`, or a single peer
    /// could occupy every slot.
    pub per_peer_share: u32,
    /// Concurrent RELAYED work items, node-wide.
    pub relay_ceiling: u32,
    /// How many distinct peers the meter tracks (SPEC §8.4).
    pub max_tracked_peers: usize,
    /// The largest attacker-chosen quantity a single request may ask for — clamped at the boundary,
    /// because work proportional to an unclamped number is unbounded work.
    pub max_request_units: u32,
}

impl Default for AdmissionLimits {
    fn default() -> Self {
        Self {
            global_ceiling: 64,
            per_peer_share: 8,
            relay_ceiling: 16,
            max_tracked_peers: 1024,
            max_request_units: 256,
        }
    }
}

/// Concurrency accounting, metered per authenticated peer.
///
/// Not a clock-based rate limiter: it counts work IN FLIGHT, so it needs no time source and stays pure
/// (SPEC §1.3). The caller pairs every successful [`AdmissionMeter::admit`] with a
/// [`AdmissionMeter::release`].
#[derive(Debug, Clone)]
pub struct AdmissionMeter {
    limits: AdmissionLimits,
    in_flight: HashMap<AuthenticatedPeer, u32>,
    global: u32,
    relayed: u32,
}

impl AdmissionMeter {
    /// A meter with no work in flight.
    #[must_use]
    pub fn new(limits: AdmissionLimits) -> Self {
        Self {
            limits,
            in_flight: HashMap::new(),
            global: 0,
            relayed: 0,
        }
    }

    /// Try to admit one unit of work for `peer`, BEFORE any of it is performed.
    ///
    /// `requested_units` is the attacker-chosen quantity the request asks for; it is clamped at this
    /// boundary rather than deeper in, where the cost would already be committed.
    ///
    /// The checks run cheapest-first, and the per-peer share is checked before the table is grown so
    /// an over-quota peer cannot expand the meter it is already exceeding.
    pub fn admit(
        &mut self,
        peer: AuthenticatedPeer,
        kind: WorkKind,
        requested_units: u32,
    ) -> Result<(), Refusal> {
        if requested_units > self.limits.max_request_units {
            return Err(Refusal::RequestTooLarge);
        }
        if self.global >= self.limits.global_ceiling {
            return Err(Refusal::GlobalCeiling);
        }
        if kind == WorkKind::Relayed && self.relayed >= self.limits.relay_ceiling {
            return Err(Refusal::RelayBudget);
        }

        let held = self.in_flight.get(&peer).copied().unwrap_or(0);
        if held >= self.limits.per_peer_share {
            return Err(Refusal::PeerShare);
        }
        if held == 0 && self.in_flight.len() >= self.limits.max_tracked_peers {
            return Err(Refusal::MeterFull);
        }

        self.in_flight.insert(peer, held + 1);
        self.global += 1;
        if kind == WorkKind::Relayed {
            self.relayed += 1;
        }
        Ok(())
    }

    /// Release one admitted unit. Removing a peer's entry at zero keeps the table proportional to
    /// ACTIVE peers rather than to every peer ever seen.
    pub fn release(&mut self, peer: AuthenticatedPeer, kind: WorkKind) {
        if let Some(held) = self.in_flight.get_mut(&peer) {
            *held = held.saturating_sub(1);
            if *held == 0 {
                self.in_flight.remove(&peer);
            }
        }
        self.global = self.global.saturating_sub(1);
        if kind == WorkKind::Relayed {
            self.relayed = self.relayed.saturating_sub(1);
        }
    }

    /// Units currently in flight node-wide.
    #[must_use]
    pub fn in_flight_total(&self) -> u32 {
        self.global
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(byte: u8) -> AuthenticatedPeer {
        AuthenticatedPeer::from_verified_session([byte; 32])
    }

    fn meter() -> AdmissionMeter {
        AdmissionMeter::new(AdmissionLimits {
            global_ceiling: 10,
            per_peer_share: 2,
            relay_ceiling: 2,
            max_tracked_peers: 3,
            max_request_units: 16,
        })
    }

    /// SPEC §8.5.6 — the load-bearing property. One peer must not consume the whole allowance. The
    /// fixture keeps the GLOBAL ceiling far above the per-peer share, so a meter that only enforced
    /// the global limit would admit every request here and an honest second peer would find nothing
    /// left.
    #[test]
    fn one_peer_cannot_consume_the_whole_allowance() {
        let mut meter = meter();
        assert!(meter.admit(peer(1), WorkKind::Own, 1).is_ok());
        assert!(meter.admit(peer(1), WorkKind::Own, 1).is_ok());
        assert_eq!(
            meter.admit(peer(1), WorkKind::Own, 1),
            Err(Refusal::PeerShare)
        );

        assert!(
            meter.admit(peer(2), WorkKind::Own, 1).is_ok(),
            "an honest peer must still be admitted after an abuser hits its share"
        );
    }

    /// SPEC §8.5.2: metering is per authenticated identity. Two distinct peers must have independent
    /// allowances — a meter that collapsed them into one bucket refuses the second peer here, which is
    /// exactly the placeholder-identity failure.
    #[test]
    fn distinct_authenticated_peers_have_independent_allowances() {
        let mut meter = meter();
        for _ in 0..2 {
            assert!(meter.admit(peer(1), WorkKind::Own, 1).is_ok());
            assert!(meter.admit(peer(2), WorkKind::Own, 1).is_ok());
        }
        assert_eq!(meter.in_flight_total(), 4);
    }

    /// SPEC §6.1.8/§8.5.3: relayed work draws on a SEPARATE budget. The fixture exhausts the relay
    /// ceiling and shows the peer can still do work for ITSELF — a shared budget refuses both.
    #[test]
    fn relayed_work_is_budgeted_apart_from_this_nodes_own() {
        let mut meter = meter();
        assert!(meter.admit(peer(1), WorkKind::Relayed, 1).is_ok());
        assert!(meter.admit(peer(2), WorkKind::Relayed, 1).is_ok());
        assert_eq!(
            meter.admit(peer(3), WorkKind::Relayed, 1),
            Err(Refusal::RelayBudget)
        );

        assert!(
            meter.admit(peer(3), WorkKind::Own, 1).is_ok(),
            "exhausting the relay budget must not stop this node's own work"
        );
    }

    /// SPEC §8.5.7: work proportional to an attacker-chosen number is clamped AT THE BOUNDARY. The
    /// refusal must happen before anything is admitted, so the meter is untouched afterwards.
    #[test]
    fn an_oversized_request_is_refused_before_anything_is_admitted() {
        let mut meter = meter();
        assert_eq!(
            meter.admit(peer(1), WorkKind::Own, u32::MAX),
            Err(Refusal::RequestTooLarge)
        );
        assert_eq!(meter.in_flight_total(), 0);
    }

    /// SPEC §8.4: the per-peer table is bounded and its behaviour at the cap is stated — an unknown
    /// peer is REFUSED, never admitted into an unbounded map.
    #[test]
    fn the_meter_table_is_bounded_and_refuses_a_new_peer_at_the_cap() {
        let mut meter = meter();
        for id in 1..=3u8 {
            assert!(meter.admit(peer(id), WorkKind::Own, 1).is_ok());
        }
        assert_eq!(
            meter.admit(peer(4), WorkKind::Own, 1),
            Err(Refusal::MeterFull)
        );
    }

    /// A peer at its cap that is ALREADY tracked must be refused on its share, not on the table size —
    /// otherwise the reported reason misleads the operator about which limit bound the node.
    #[test]
    fn an_over_quota_known_peer_is_refused_on_its_share_not_on_the_table() {
        let mut meter = meter();
        for id in 1..=3u8 {
            assert!(meter.admit(peer(id), WorkKind::Own, 1).is_ok());
        }
        assert!(meter.admit(peer(1), WorkKind::Own, 1).is_ok());
        assert_eq!(
            meter.admit(peer(1), WorkKind::Own, 1),
            Err(Refusal::PeerShare)
        );
    }

    /// Releasing frees the allowance again, and drops the peer's entry so the table tracks ACTIVE
    /// peers rather than every peer ever seen.
    #[test]
    fn releasing_restores_the_allowance_and_shrinks_the_table() {
        let mut meter = meter();
        for id in 1..=3u8 {
            assert!(meter.admit(peer(id), WorkKind::Own, 1).is_ok());
        }
        assert_eq!(
            meter.admit(peer(4), WorkKind::Own, 1),
            Err(Refusal::MeterFull)
        );

        meter.release(peer(1), WorkKind::Own);
        assert!(meter.admit(peer(4), WorkKind::Own, 1).is_ok());
        assert_eq!(meter.in_flight_total(), 3);
    }

    /// SPEC §8.5.5: refusal is NAMED, never a silent slowdown. Each limit reports its own reason, so
    /// an operator can tell which bound was reached rather than seeing one undifferentiated failure.
    #[test]
    fn each_limit_sheds_load_under_its_own_name() {
        let mut narrow = AdmissionMeter::new(AdmissionLimits {
            global_ceiling: 1,
            per_peer_share: 1,
            relay_ceiling: 1,
            max_tracked_peers: 8,
            max_request_units: 4,
        });
        assert!(narrow.admit(peer(1), WorkKind::Own, 1).is_ok());
        assert_eq!(
            narrow.admit(peer(2), WorkKind::Own, 1),
            Err(Refusal::GlobalCeiling)
        );
        assert_eq!(
            narrow.admit(peer(2), WorkKind::Own, 99),
            Err(Refusal::RequestTooLarge),
            "the request-size clamp is checked before the ceiling, so cost is bounded first"
        );
    }
}
