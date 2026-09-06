pub mod client;
pub mod input;
pub mod messages;
pub mod networked_registry;
pub mod prelude;
pub mod server;
pub mod snapshot;

use bevy::prelude::*;

use bevy_ticked::{
    registry::TickedComponentRegistry,
    tick::{CurrentTick, TicksPaused},
    tracked_entity::{TickTrackedEntity, TickTrackedEntityCounter},
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
/// `reset_on_join` *sets* `TicksPaused` and leaves the client waiting for a first snapshot. A peer
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
    world.insert_resource(TickTrackedEntityCounter::default());
    world.resource_mut::<InputQueue<T>>().inputs.clear();
    let registry = world.resource::<TickedComponentRegistry>().clone();
    registry.clear_all(world);

    // A peer that leaves mid-sync would otherwise stay paused for ever, waiting for a snapshot
    // from a session it is no longer in.
    world.remove_resource::<TicksPaused>();

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
}
