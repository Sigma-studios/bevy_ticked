pub mod client;
pub mod delta;
pub mod diagnostics;
pub mod input;
pub mod input_plugin;
pub mod messages;
pub mod networked_registry;
pub mod pause;
pub mod prelude;
pub mod replication;
pub mod server;
pub mod smoothing;
pub mod snapshot;

use bevy::prelude::*;

use bevy_ticked::{
    registry::TickedComponentRegistry,
    resource_registry::TickedResourceRegistry,
    tick::{CurrentTick, TickHoldReason, TickHolds},
    tracked_entity::{LocalSpawnerSlot, TickTrackedEntity, TrackedIdAllocator},
};

use crate::input::{InputQueue, TickedInput};

/// Undo a session, so the next one does not inherit it.
///
/// Entering a session has had a reset at each door for a long time —
/// [`reset_on_host`](server::reset_on_host) raises the entity counter to the world's high-water
/// mark, [`reset_on_join`](client::reset_on_join) despawns and zeroes — and **leaving had none**.
/// Removing the role resources removed the role and nothing else: the input queue kept the
/// departed session's inputs, `CurrentTick` kept counting from wherever it stopped, and every
/// entity the host had sent was still standing when the next world arrived.
///
/// So a game that can leave a lobby had to clean up after this crate, in a place this crate never
/// told it about. Both consumers that can leave one wrote it themselves, in two places each,
/// because a *session* ending and a *world* ending are different events; the one that does not
/// clean up has the departed session's inputs applied to the next one, which presents as a body
/// twitching for a second on entry and is nearly impossible to attribute.
///
/// This is the third door, and it is deliberately the union of the other two: despawn what was
/// tracked (a client's rule — nothing here has a claim on the old world any more), clear the
/// queue, clear the history, zero the tick, and un-pause.
///
/// The un-pause is the one that is not symmetric with anything, and it is the one that bites:
/// `reset_on_join` holds `AwaitingSync` and leaves the client waiting for a first snapshot. A peer
/// that leaves before that snapshot arrives — a refused join, a host that quits during the
/// handshake — would otherwise sit paused for ever, waiting on a session it is no longer in, in a
/// world whose clock has stopped for no reason it can see.
pub fn reset_on_leave<T: TickedInput>(world: &mut World) {
    let stale: Vec<Entity> = {
        let mut tracked = world.query_filtered::<Entity, With<TickTrackedEntity>>();
        tracked.iter(world).collect()
    };
    for entity in stale {
        world.despawn(entity);
    }

    world.insert_resource(CurrentTick(0));
    world.insert_resource(TrackedIdAllocator::default());
    world.remove_resource::<LocalSpawnerSlot>();
    world.insert_resource(input_plugin::LocalPlayer::default());
    world.resource_mut::<InputQueue<T>>().inputs.clear();
    let registry = world.resource::<TickedComponentRegistry>().clone();
    registry.clear_all(world);
    // Clearing a resource's history is not the same as clearing the resource: a `RoundState`
    // that said "round 7, team B leads" said it into the next lobby too, and every consumer
    // wrote a `reset_round_on_leave` to put it back. Registered means "part of the session",
    // and the session is over.
    if let Some(resources) = world.get_resource::<TickedResourceRegistry>().cloned() {
        resources.reset_all(world);
    }

    // A peer that leaves mid-sync would otherwise stay held for ever, waiting for a snapshot
    // from a session it is no longer in. Only the session's own reasons: a game's pause menu
    // is still open.
    let mut holds = world.resource_mut::<TickHolds>();
    holds.release(TickHoldReason::AwaitingSync);
    holds.release(TickHoldReason::SoftHold);
    holds.release(TickHoldReason::SessionPause);

    // The clock restarts at zero, so anything remembering *which tick* it last saw from a peer
    // has to forget it too — otherwise the next session's first inputs all read as older than
    // the last session's last ones. Cleared in place rather than inserted, so a client-only app
    // that never added the server plugin does not grow the server's resources on its way out.
    if let Some(mut newest) = world.get_resource_mut::<server::NewestInputTick>() {
        newest.0.clear();
    }
    if let Some(mut margins) = world.get_resource_mut::<server::InputMargins>() {
        margins.0.clear();
    }
    // And the same on the receiving side: the last snapshot tick a client applied would
    // otherwise make every snapshot of the next session read as older than it.
    if let Some(mut applied) = world.get_resource_mut::<client::AppliedSnapshotTick>() {
        applied.0 = None;
    }
    if let Some(mut seq) = world.get_resource_mut::<client::LastAppliedSeq>() {
        seq.0 = None;
    }
    if let Some(mut history) = world.get_resource_mut::<replication::AuthoritativeHistory>() {
        history.clear();
    }
    if let Some(mut display) = world.get_resource_mut::<replication::DisplayTick>() {
        display.0 = None;
    }
    if let Some(mut seqs) = world.get_resource_mut::<server::SnapshotSeq>() {
        seqs.0.clear();
    }
    if let Some(mut acks) = world.get_resource_mut::<server::LastAck>() {
        acks.0.clear();
    }
    if let Some(mut baselines) = world.get_resource_mut::<delta::Baselines>() {
        baselines.0.clear();
    }
    if let Some(mut nack) = world.get_resource_mut::<client::NackFull>() {
        nack.0 = false;
    }
}
