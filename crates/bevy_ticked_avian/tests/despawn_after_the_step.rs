//! A body tombstoned after the step, while it touches others, replays bit-identically.
//!
//! Since avian 0.8 the constraint graph is not changed when a collider goes: its contacts are
//! queued (`ContactStatusChangeQueue`) and taken out of the graph when the next step starts. A
//! despawn in `AfterPhysics` — a projectile's hit, a pickup — leaves that queue full across the
//! tick boundary, where the history captures the graphs and a rollback restores them. Both
//! windows here cross it: one restores a tick before the tombstone, the other the tick of it.

use avian3d::prelude::*;
use bevy::prelude::*;
use bevy_ticked::TickedSimulation;
use bevy_ticked::lifetimes::TickedEntityCommandsExt;
use bevy_ticked::prelude::*;
use bevy_ticked::tracked_entity::TickTrackedEntity;
use bevy_ticked_testing::fixtures::avian::{self, AvianHash, TickedSimulationSet, spawn_stack};
use bevy_ticked_testing::prelude::*;

const BOXES: usize = 6;
/// Mid-topple: every box is touching another or the ground.
const TOMBSTONE_AT: Tick = Tick(60);

/// Tombstones the highest-numbered box on [`TOMBSTONE_AT`], after the step.
fn tombstone_a_box(
    tick: Res<CurrentTick>,
    boxes: Query<(Entity, &TickTrackedEntity), With<RigidBody>>,
    mut commands: Commands,
) {
    if tick.0 != TOMBSTONE_AT {
        return;
    }
    if let Some((entity, _)) = boxes.iter().max_by_key(|(_, id)| id.0) {
        commands.entity(entity).despawn_ticked();
    }
}

fn app() -> App {
    let mut app = peer_app(HOST_UUID, |app| {
        avian::install(app);
        app.add_systems(
            TickedSimulation,
            tombstone_a_box.in_set(TickedSimulationSet::AfterPhysics),
        );
    });
    spawn_stack(app.world_mut(), BOXES);
    app
}

fn assert_a_box_was_tombstoned(app: &App) {
    // A tombstone is `Disabled`, which a query skips; walk the entities instead.
    let tombstoned = app
        .world()
        .iter_entities()
        .filter(|entity| entity.contains::<RigidBody>() && entity.contains::<Tombstone>())
        .count();
    assert_eq!(tombstoned, 1, "the window did not contain the tombstone");
}

#[test]
fn a_rewind_across_a_tombstone_after_the_step_replays_identically() {
    let mut app = app();
    assert_replays_identically::<AvianHash>(&mut app, TOMBSTONE_AT - Ticks(10), Ticks(48));
    assert_a_box_was_tombstoned(&app);
}

#[test]
fn a_rewind_to_the_tick_of_a_tombstone_after_the_step_replays_identically() {
    let mut app = app();
    assert_replays_identically::<AvianHash>(&mut app, TOMBSTONE_AT, Ticks(48));
    assert_a_box_was_tombstoned(&app);
}
