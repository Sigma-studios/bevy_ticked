//! What a rewind does to entities that were spawned or despawned after the target tick.
//!
//! These used to pin the opposite: a rewind past a spawn left a husk, and a rewind past a
//! despawn could not bring the entity back. Existence is in the history now.

use std::time::Duration;

use bevy::ecs::entity_disabling::Disabled;
use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ticked::prelude::*;
use bevy_ticked::registry::TickedComponentRegistry;
use bevy_ticked::tracked_entity::TickTrackedEntity;
use bevy_ticked::tracked_index::TrackedEntityIndex;

#[derive(Component, Clone, Copy, Debug, PartialEq)]
struct Height(i32);

#[derive(Component, Clone, Copy, Debug, PartialEq)]
struct Tag;

/// Local-only, put on by the game's spawn observer: the thing a rebuild has to redo.
#[derive(Component, Debug, Default)]
struct Dressed;

fn dress(add: On<Add, TickTrackedEntity>, mut commands: Commands) {
    commands.entity(add.entity).insert(Dressed);
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(TickedPlugin {
            source: TickSource::Manual,
            ..default()
        })
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 64.0,
        )))
        .register_ticked_component::<Height>()
        .register_ticked_component::<Tag>()
        .add_observer(dress);
    app
}

/// Advance one tick by hand, the way `StepForward` does.
fn step(app: &mut App) {
    app.world_mut().write_message(StepForward);
    app.update();
}

fn restore(app: &mut App, tick: u64) {
    let registry = app.world().resource::<TickedComponentRegistry>().clone();
    registry.restore_all(app.world_mut(), tick);
}

fn tracked_ids(app: &mut App) -> Vec<u64> {
    let mut q = app.world_mut().query::<&TickTrackedEntity>();
    let mut ids: Vec<u64> = q.iter(app.world()).map(|t| t.0).collect();
    ids.sort_unstable();
    ids
}

#[test]
fn rewinding_past_a_spawn_despawns_it() {
    let mut app = app();
    app.world_mut().spawn_tracked((Height(10), Tag));
    step(&mut app);
    step(&mut app);

    // A second entity appears at tick 2 -- a cannon being planted, a rope landing.
    let late = app.world_mut().spawn_tracked((Height(99), Tag));
    step(&mut app);
    assert_eq!(tracked_ids(&mut app).len(), 2);

    restore(&mut app, 1);

    assert_eq!(
        tracked_ids(&mut app).len(),
        1,
        "the entity that did not exist at tick 1 is gone from every query"
    );
    assert!(
        app.world().get_entity(late).is_ok() && app.world().get::<Disabled>(late).is_some(),
        "kept as a tombstone, so a replay that spawns it again gets the same entity"
    );
    assert!(
        app.world().get::<Height>(late).is_some(),
        "with its state intact, not stripped"
    );
}

#[test]
fn a_despawn_rolled_back_resurrects_the_same_entity_id() {
    let mut app = app();
    let body = app.world_mut().spawn_tracked((Height(10), Tag));
    step(&mut app);
    step(&mut app);

    app.world_mut().entity_mut(body).despawn_ticked();
    step(&mut app);
    assert!(
        tracked_ids(&mut app).is_empty(),
        "despawned, so absent from every query"
    );
    assert_eq!(
        app.world().resource::<TrackedEntityIndex>().get(1 << 8),
        None,
        "and unindexed"
    );

    restore(&mut app, 2);

    assert_eq!(
        tracked_ids(&mut app).len(),
        1,
        "alive at tick 2, so alive again"
    );
    let mut q = app.world_mut().query::<(Entity, &Height)>();
    let (entity, height) = q.single(app.world()).unwrap();
    assert_eq!(entity, body, "the same entity, not a copy");
    assert_eq!(*height, Height(10));
    assert!(app.world().get::<Disabled>(body).is_none());
}

#[test]
fn a_tombstone_reused_by_a_replayed_spawn_keeps_the_entity_id() {
    let mut app = app();
    app.world_mut().spawn_tracked(Height(1));
    step(&mut app);
    let bullet = app.world_mut().spawn_tracked((Height(5), Tag));
    let bullet_id = app.world().get::<TickTrackedEntity>(bullet).unwrap().0;
    step(&mut app);

    // Rewind past the bullet's spawn, then replay the spawn: the allocator was rolled back
    // too, so it mints the same id and revives the tombstone.
    rollback_to_tick(app.world_mut(), 1);
    assert!(app.world().get::<Disabled>(bullet).is_some());
    let again = app.world_mut().spawn_tracked((Height(6), Tag));
    assert_eq!(
        again, bullet,
        "the replayed spawn is the entity the game already held"
    );
    assert_eq!(
        app.world().get::<TickTrackedEntity>(again).unwrap().0,
        bullet_id
    );
    assert!(app.world().get::<Disabled>(again).is_none());
    assert_eq!(app.world().get::<Height>(again), Some(&Height(6)));
}

#[test]
fn restoring_a_tick_where_an_entity_was_alive_rebuilds_it_through_the_spawn_path() {
    let mut app = app();
    let body = app.world_mut().spawn_tracked((Height(10), Tag));
    step(&mut app);
    step(&mut app);
    let id = app.world().get::<TickTrackedEntity>(body).unwrap().0;

    // A plain despawn: destroyed, not tombstoned. The observer records the death.
    app.world_mut().despawn(body);
    step(&mut app);
    assert!(tracked_ids(&mut app).is_empty());
    let lifetime = app
        .world()
        .resource::<TrackedEntityLifetimes>()
        .lifetime(id)
        .expect("still in the lifetimes");
    assert_eq!(
        lifetime.died,
        Some(3),
        "alive at tick 2 (captured), gone from tick 3"
    );

    restore(&mut app, 2);

    let mut q = app
        .world_mut()
        .query::<(Entity, &TickTrackedEntity, &Height, Has<Tag>, Has<Dressed>)>();
    let (entity, tracked, height, tag, dressed) = q.single(app.world()).unwrap();
    assert_ne!(entity, body, "a fresh entity: the old one is gone for good");
    assert_eq!(tracked.0, id, "under the same id");
    assert_eq!(*height, Height(10));
    assert!(tag, "every registered component is back from the history");
    assert!(dressed, "and the game's On<Add> observer dressed it");
}

#[test]
fn a_plain_despawn_on_a_tracked_entity_is_recorded_and_warns_once() {
    let mut app = app();
    let first = app.world_mut().spawn_tracked(Height(1));
    let second = app.world_mut().spawn_tracked(Height(2));
    step(&mut app);
    let (a, b) = (
        app.world().get::<TickTrackedEntity>(first).unwrap().0,
        app.world().get::<TickTrackedEntity>(second).unwrap().0,
    );
    app.world_mut().despawn(first);
    app.world_mut().despawn(second);
    let lifetimes = app.world().resource::<TrackedEntityLifetimes>();
    assert_eq!(
        lifetimes.lifetime(a).unwrap().died,
        Some(2),
        "captured alive at 1"
    );
    assert_eq!(lifetimes.lifetime(b).unwrap().died, Some(2));
    // The warning is `warn_once!`; this test asserts the recording, the log is for the eye.
}

#[test]
fn an_entity_with_no_registered_components_still_exists_in_the_lifetimes() {
    let mut app = app();
    app.world_mut().spawn_tracked(Height(10));
    // Tracked from the start, but carrying nothing registered.
    let bare = app.world_mut().spawn_tracked(());
    let bare_id = app.world().get::<TickTrackedEntity>(bare).unwrap().0;
    step(&mut app);
    step(&mut app);

    assert_eq!(
        app.world()
            .resource::<TrackedEntityLifetimes>()
            .alive_at(1, bare_id),
        Some(true),
        "no saved state is not no entity"
    );
    restore(&mut app, 1);
    assert!(
        app.world().get_entity(bare).is_ok() && app.world().get::<Disabled>(bare).is_none(),
        "the bare entity existed at tick 1 and still does"
    );
}

#[test]
fn a_tombstone_is_reaped_once_the_window_has_passed() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(TickedPlugin {
            source: TickSource::Manual,
            history_ticks: Some(4),
            ..default()
        })
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 64.0,
        )))
        .register_ticked_component::<Height>();
    let body = app.world_mut().spawn_tracked(Height(1));
    step(&mut app);
    app.world_mut().entity_mut(body).despawn_ticked();
    for _ in 0..3 {
        step(&mut app);
        assert!(
            app.world().get_entity(body).is_ok(),
            "inside the window it is kept"
        );
    }
    for _ in 0..4 {
        step(&mut app);
    }
    assert!(
        app.world().get_entity(body).is_err(),
        "past the window nothing can ask for it back, so it is destroyed"
    );
}
