//! Making a specific thing go wrong, so that a test's failure is the test's decision.
//!
//! A seeded `Link` produces faults from a distribution; these produce one on demand. The
//! difference is what the test can claim afterwards: "the snapshot at frame 40 was the one
//! lost" rather than "some snapshots were lost", and "the client's copy of the body was wrong
//! and the next snapshot fixed it" rather than "it converged eventually".
//!
//! The fuzzers at the bottom — [`garbage`], [`truncations`], [`bit_flips`] — exist for one
//! claim: **a decoder never panics on bytes from the network.** A malformed packet is logged and
//! skipped; a panic on one is a remote crash for every peer that receives it. They are seeded, so
//! the packet that found a panic is the packet that reproduces it.

use bevy::ecs::component::Mutable;
use bevy::prelude::*;
use bevy_ensemble::{EnsembleMessage, EnsembleMessageRegistry, encode_ensemble_message};
use bevy_ensemble_loopback::{PeerId, SeededRng};

use crate::net::TickedNetwork;
use crate::view::tracked_entity;

/// Rewrite `T` on the entity carrying tracked id `id`, in place, between frames.
///
/// The way to make a replica wrong on purpose: what the next snapshot does about it is then the
/// thing under test.
///
/// # Panics
///
/// If no entity carries `id`, or it has no `T`. Both mean the test is corrupting the wrong
/// thing, and a silent no-op there would make every assertion after it vacuous.
pub fn corrupt_component<T: Component<Mutability = Mutable>>(
    app: &mut App,
    id: u64,
    rewrite: impl FnOnce(&mut T),
) {
    let entity = tracked_entity(app, id)
        .unwrap_or_else(|| panic!("no entity on this peer carries tracked id {id}"));
    let mut component = app.world_mut().get_mut::<T>(entity).unwrap_or_else(|| {
        panic!(
            "tracked id {id} has no `{}` to corrupt",
            std::any::type_name::<T>()
        )
    });
    rewrite(&mut component);
}

/// Rewrite the next packet from `from` to `to` before the link sees it. Queues: two calls
/// corrupt the next two.
pub fn corrupt_next_packet(
    net: &mut TickedNetwork,
    from: PeerId,
    to: PeerId,
    rewrite: impl FnOnce(&mut Vec<u8>) + 'static,
) {
    net.net.corrupt_next(from, to, rewrite);
}

/// Lose exactly the next `count` unreliable packets from `from` to `to`. Reliable packets pass:
/// a reliable transport retransmits rather than loses.
pub fn drop_next_packets(net: &mut TickedNetwork, from: PeerId, to: PeerId, count: usize) {
    net.net.drop_next(from, to, count);
}

/// Encode `message` exactly as `app` would send it: its registry's wire index, then postcard.
///
/// For crafting a packet the game would never send — a snapshot for a tick in the past, an
/// input from a player who is not in the session — and handing it to [`deliver_raw`].
///
/// # Panics
///
/// If `T` is not registered on `app`.
pub fn encode_as<T: EnsembleMessage>(app: &App, message: &T) -> Vec<u8> {
    let registry = app
        .world()
        .get_resource::<EnsembleMessageRegistry>()
        .expect("EnsemblePlugin is not on this peer, so it has no message registry");
    encode_ensemble_message(registry, message)
}

/// Put `bytes` straight into `to`'s inbox for the next frame, as if `sender` had sent them.
/// Bypasses the links, the trace and the counters.
pub fn deliver_raw(net: &mut TickedNetwork, to: PeerId, sender: u128, bytes: Vec<u8>) {
    net.net.deliver_raw(to, sender, bytes);
}

/// Every proper prefix of `packet`, shortest first: the packet cut off after 0, 1, 2 … bytes.
///
/// A decoder that reads a length and then trusts it is found by exactly one of these.
pub fn truncations(packet: &[u8]) -> impl Iterator<Item = &[u8]> {
    (0..packet.len()).map(move |length| &packet[..length])
}

/// `count` packets of random bytes, each up to `max_len` long, from a seeded generator.
pub fn garbage(seed: u64, count: usize, max_len: usize) -> Vec<Vec<u8>> {
    let mut rng = SeededRng::new(seed);
    (0..count)
        .map(|_| {
            let length = rng.below(max_len as u64 + 1) as usize;
            (0..length).map(|_| rng.next_u64() as u8).collect()
        })
        .collect()
}

/// `count` copies of `packet`, each with one randomly chosen bit flipped. Empty for an empty
/// packet, which has no bit to flip.
pub fn bit_flips(packet: &[u8], seed: u64, count: usize) -> Vec<Vec<u8>> {
    if packet.is_empty() {
        return Vec::new();
    }
    let mut rng = SeededRng::new(seed);
    let bits = packet.len() as u64 * 8;
    (0..count)
        .map(|_| {
            let bit = rng.below(bits);
            let mut flipped = packet.to_vec();
            flipped[(bit / 8) as usize] ^= 1 << (bit % 8);
            flipped
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncations_are_every_proper_prefix() {
        let packet = [1u8, 2, 3];
        let prefixes: Vec<&[u8]> = truncations(&packet).collect();
        assert_eq!(prefixes, vec![&[][..], &[1][..], &[1, 2][..]]);
    }

    #[test]
    fn a_bit_flip_changes_exactly_one_bit() {
        let packet = [0u8; 8];
        for flipped in bit_flips(&packet, 3, 20) {
            let ones: u32 = flipped.iter().map(|byte| byte.count_ones()).sum();
            assert_eq!(ones, 1, "{flipped:?}");
        }
    }

    #[test]
    fn the_fuzzers_replay_from_their_seed() {
        assert_eq!(garbage(9, 10, 32), garbage(9, 10, 32));
        assert_ne!(garbage(9, 10, 32), garbage(10, 10, 32));
    }
}
