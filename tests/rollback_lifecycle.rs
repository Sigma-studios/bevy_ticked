//! What `restore_all` does to entities that did not exist at the target tick.
//!
//! These began as the executable half of `shooting_ropes/docs/upstream-needs.md` §2.2 and §2.5 —
//! each one stating a claim from that document and either holding it up or knocking it down.
//! §2.5 is now **closed**: a rewind past a spawn despawns the entity rather than hollowing it out.
//! The tests are kept in place, inverted where the behaviour inverted, because the claims they
//! encode are exactly the ones a regression would quietly undo.
//!
//! The one that did *not* invert is the last one, and it is the reason this took a separate
//! lifetime history rather than a heuristic over the component histories.

use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ticked::lifetimes::TrackedEntityLifetimes;
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

/// §2.2, first half — **inverted**. Rollback now undoes a spawn.
#[test]
fn rewinding_past_a_spawn_despawns_it() {
    let mut app = app();

    // An entity that exists from the start, so tick 0 is captured.
    app.world_mut().spawn((TickTrackedEntity(1), Height(10)));
    step(&mut app);
    step(&mut app);

    // A second entity appears at tick 2 — a cannon being planted, a rope landing.
    let late = app.world_mut().spawn((TickTrackedEntity(2), Height(99))).id();
    step(&mut app);

    restore(&mut app, 1);

    assert!(
        app.world().get_entity(late).is_err(),
        "an entity that did not exist at tick 1 must not survive a rewind to tick 1"
    );
}

/// §2.5 — **closed**. There is no husk left to inspect.
///
/// This used to assert the failure the document called the worst possible property for a
/// determinism harness: an entity stripped of every registered component, still tracked, still
/// drawn, and captured into every future tick for the rest of the session. What replaces it is the
/// assertion that no such thing is left behind.
#[test]
fn rewinding_past_a_spawn_leaves_no_husk() {
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

    assert!(
        app.world().get_entity(late).is_err(),
        "the husk is gone rather than hollowed out"
    );

    // And the entity that *did* exist at tick 1 is untouched by the sweep.
    let mut tracked = app.world_mut().query::<(&TickTrackedEntity, &Height)>();
    let survivors: Vec<(u64, i32)> = tracked
        .iter(app.world())
        .map(|(tracked, height)| (tracked.0, height.0))
        .collect();
    assert_eq!(
        survivors,
        vec![(1, 10)],
        "the survivor keeps both its identity and its restored state"
    );
}

/// Despawns are still one-way, and that is not an oversight.
///
/// An entity's unregistered parts — its collider, its mesh, the relationships it stood in — were
/// never in the history, so there is nothing to rebuild it from. Spawns roll back; despawns do
/// not. Anything that needs a despawn undone has to model it as state on a permanent entity.
#[test]
fn rewinding_past_a_despawn_does_not_resurrect_it() {
    let mut app = app();

    app.world_mut().spawn((TickTrackedEntity(1), Height(10)));
    let doomed = app.world_mut().spawn((TickTrackedEntity(2), Height(20))).id();
    step(&mut app);
    step(&mut app);

    app.world_mut().despawn(doomed);
    step(&mut app);

    restore(&mut app, 1);

    let mut tracked = app.world_mut().query::<&TickTrackedEntity>();
    let ids: Vec<u64> = tracked.iter(app.world()).map(|tracked| tracked.0).collect();
    assert_eq!(
        ids,
        vec![1],
        "net 2 existed at tick 1 but cannot come back — restore has no way to rebuild it"
    );
}

/// The counterweight that ruled out every cheaper approach.
///
/// An entity may legitimately carry none of the registered types at a tick, so "has no saved
/// state" cannot mean "did not exist" — a despawn-what-has-no-state heuristic would kill it. The
/// lifetime history is what tells the two apart, and this is the test that says so.
#[test]
fn an_entity_with_no_registered_components_is_not_mistaken_for_one_that_did_not_exist() {
    let mut app = app();

    app.world_mut().spawn((TickTrackedEntity(1), Height(10)));
    // Tracked from the start, but carrying nothing registered until later.
    let bare = app.world_mut().spawn(TickTrackedEntity(2)).id();
    step(&mut app);

    // It has no saved state at tick 1 — in any component history.
    {
        use bevy_ticked::world_actions::WorldActions;
        let world = app.world();
        let in_any_component_history = world
            .resource::<WorldActions<Height>>()
            .at_tick(1)
            .is_some_and(|state| state.contains_key(&2))
            || world
                .resource::<WorldActions<Tag>>()
                .at_tick(1)
                .is_some_and(|state| state.contains_key(&2));
        assert!(
            !in_any_component_history,
            "an entity with no registered components has no saved state at all — \
             so 'no saved state' cannot mean 'did not exist'"
        );
    }

    // But the lifetime history knows perfectly well that it was there.
    assert_eq!(
        app.world()
            .resource::<TrackedEntityLifetimes>()
            .existed(1, 2),
        Some(true),
        "existence is recorded independently of any component, which is the whole point"
    );

    // So it survives the rewind.
    step(&mut app);
    restore(&mut app, 1);
    assert!(
        app.world().get_entity(bare).is_ok(),
        "the entity that had nothing to save is still an entity that existed"
    );
}

/// Restoring a tick nobody captured must not be read as "the world was empty then".
///
/// An absent lifetime record and an empty one are different facts, and conflating them despawns
/// every tracked entity in the world. This is the safe direction, asserted.
#[test]
fn restoring_an_uncaptured_tick_despawns_nothing() {
    let mut app = app();

    app.world_mut().spawn((TickTrackedEntity(1), Height(10)));
    step(&mut app);

    // Far beyond anything captured.
    restore(&mut app, 9_999);

    let mut tracked = app.world_mut().query::<&TickTrackedEntity>();
    assert_eq!(
        tracked.iter(app.world()).count(),
        1,
        "an unknown tick is a no-op, not an empty world"
    );
}
