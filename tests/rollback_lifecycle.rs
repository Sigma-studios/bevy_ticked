//! What `restore_all` does to entities that did not exist at the target tick.
//!
//! These are the executable half of `shooting_ropes/docs/upstream-needs.md` §2.2
//! and §2.5. Each one states a claim from that document and either holds it up or
//! knocks it down; a claim nobody can reproduce is not a claim.

use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ticked::prelude::*;
use bevy_ticked::registry::TickedComponentRegistry;
use bevy_ticked::tracked_entity::TickTrackedEntity;

#[derive(Component, Clone, Copy, Debug, PartialEq)]
struct Height(i32);

#[derive(Component, Clone, Copy, Debug, PartialEq)]
struct Tag;

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
        .register_ticked_component::<Tag>();
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

/// §2.2, first half: rollback never despawns.
#[test]
fn rewinding_past_a_spawn_does_not_despawn_it() {
    let mut app = app();

    // An entity that exists from the start, so tick 0 is captured (the crate
    // refuses to capture an empty world).
    app.world_mut().spawn((TickTrackedEntity(1), Height(10)));
    step(&mut app);
    step(&mut app);

    // A second entity appears at tick 2 -- a cannon being planted, a rope landing.
    let late = app.world_mut().spawn((TickTrackedEntity(2), Height(99))).id();
    step(&mut app);

    restore(&mut app, 1);

    assert!(
        app.world().get_entity(late).is_ok(),
        "restore_all despawned an entity that did not exist at the target tick -- \
         if this ever fails, §2.2 and §2.5 are fixed and the workaround can go"
    );
}

/// §2.5: what is left behind is a husk -- still tracked, no registered state.
///
/// This is the failure the document calls the worst possible property for a
/// determinism harness to have, and it is exactly reproducible.
#[test]
fn rewinding_past_a_spawn_strips_every_registered_component() {
    let mut app = app();

    app.world_mut().spawn((TickTrackedEntity(1), Height(10)));
    step(&mut app);
    step(&mut app);

    let late = app
        .world_mut()
        .spawn((TickTrackedEntity(2), Height(99), Tag, Name::new("cannon")))
        .id();
    step(&mut app);

    restore(&mut app, 1);

    let husk = app.world().entity(late);
    assert!(
        husk.get::<Height>().is_none(),
        "the saved map for tick 1 has no entry for net id 2, so restore removes Height"
    );
    assert!(
        husk.get::<Tag>().is_none(),
        "every registered type is stripped, not just the one that changed"
    );
    // And what survives is precisely what makes it invisible: it is still tracked,
    // still named, and will be captured into every future tick as a component-less
    // entity.
    assert!(husk.get::<TickTrackedEntity>().is_some(), "still tracked");
    assert!(husk.get::<Name>().is_some(), "still carries its unregistered parts");
}

/// The cheap mitigation the document does not consider: register
/// `TickTrackedEntity` itself.
///
/// If the tracking marker is a registered component, restoring past the spawn
/// removes it, so the husk drops out of the tracked set instead of being captured
/// and serialised forever. It is not a fix -- the entity still leaks -- but it
/// bounds the damage to one leaked entity rather than a permanently divergent
/// world, and it costs a consumer one line.
#[test]
fn registering_the_tracking_marker_untracks_the_husk() {
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
        .register_ticked_component::<TickTrackedEntity>();

    app.world_mut().spawn((TickTrackedEntity(1), Height(10)));
    step(&mut app);
    step(&mut app);

    let late = app.world_mut().spawn((TickTrackedEntity(2), Height(99))).id();
    step(&mut app);

    restore(&mut app, 1);

    assert!(
        app.world().entity(late).get::<TickTrackedEntity>().is_none(),
        "with the marker registered, the husk stops being tracked"
    );
    // The survivor is untouched.
    let mut tracked = app.world_mut().query::<&TickTrackedEntity>();
    let ids: Vec<u64> = tracked.iter(app.world()).map(|t| t.0).collect();
    assert_eq!(ids, vec![1], "only the entity that existed at tick 1 is tracked");
}

/// The counterweight, and the reason the mitigation above is not a fix: an entity
/// that legitimately carries none of the registered types at the target tick is
/// indistinguishable from one that did not exist. Any "despawn what has no saved
/// state" heuristic would kill it.
#[test]
fn an_entity_with_no_registered_components_looks_exactly_like_one_that_did_not_exist() {
    let mut app = app();

    app.world_mut().spawn((TickTrackedEntity(1), Height(10)));
    // Tracked from the start, but carrying nothing registered until later.
    let bare = app.world_mut().spawn(TickTrackedEntity(2)).id();
    step(&mut app);

    let registry = app.world().resource::<TickedComponentRegistry>().clone();
    let world = app.world_mut();
    let has_any = {
        use bevy_ticked::world_actions::WorldActions;
        world.resource::<WorldActions<Height>>().at_tick(1).unwrap().contains_key(&2)
            || world.resource::<WorldActions<Tag>>().at_tick(1).unwrap().contains_key(&2)
    };
    assert!(
        !has_any,
        "an entity with no registered components has no saved state at all -- \
         so 'no saved state' cannot mean 'did not exist'"
    );
    let _ = (registry, bare);
}
