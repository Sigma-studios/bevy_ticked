//! The registration handshake (§2.3) and per-tick events under rollback (§2.4).

use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ticked::prelude::*;
use bevy_ticked::registry::TickedComponentRegistry;
use bevy_ticked::tracked_entity::TickTrackedEntity;

#[derive(Component, Clone, Copy, Debug, PartialEq)]
struct Position(i32);

#[derive(Component, Clone, Copy, Debug, PartialEq)]
struct Velocity(i32);

#[derive(Clone, Copy, Debug, PartialEq)]
struct Thunk(u32);

fn registry(register: impl Fn(&mut App)) -> TickedComponentRegistry {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    register(&mut app);
    app.world().resource::<TickedComponentRegistry>().clone()
}

// ── §2.3 ─────────────────────────────────────────────────────────────────────
//
// The wire hash is over the *networked* names, sorted. `tests/wire_hash.rs` pins that format
// in full; what stays here is the property this file was written for: rollback-only
// registrations are outside it, in every order and every number.

/// A rollback-only type never reaches a snapshot, so no ordering of them is a wire format.
#[test]
fn swapping_two_rollback_only_types_is_invisible_and_harmless() {
    let forward = registry(|app| {
        app.register_ticked_component::<Position>();
        app.register_ticked_component::<Velocity>();
    });
    let reversed = registry(|app| {
        app.register_ticked_component::<Velocity>();
        app.register_ticked_component::<Position>();
    });

    assert_eq!(
        forward.wire_hash(),
        reversed.wire_hash(),
        "neither index reaches a snapshot, so neither ordering is a wire format"
    );
}

/// And neither is their number: an extra local-only type on one peer is not a mismatch.
#[test]
fn an_extra_rollback_only_type_is_not_a_mismatch() {
    let short = registry(|app| {
        app.register_ticked_component_as::<Position>("Position");
    });
    let long = registry(|app| {
        app.register_ticked_component_as::<Position>("Position");
        app.register_ticked_component_as::<Velocity>("Velocity");
    });
    assert_eq!(short.wire_hash(), long.wire_hash());
    assert_eq!(long.wire_names().count(), 0, "nothing here is on the wire");
    assert_eq!(long.registered_names().count(), 2, "but both are registered");
}

#[test]
fn the_wire_hash_is_stable_for_the_same_registration() {
    let a = registry(|app| {
        app.register_ticked_component::<Position>();
        app.register_ticked_component::<Velocity>();
    });
    let b = registry(|app| {
        app.register_ticked_component::<Position>();
        app.register_ticked_component::<Velocity>();
    });
    assert_eq!(a.wire_hash(), b.wire_hash(), "two peers must agree");
}

// ── §2.4 ─────────────────────────────────────────────────────────────────────

#[derive(Resource, Default)]
struct Presented(Vec<(u64, Thunk)>);

/// A world that fires one event on tick 3 and nothing otherwise, presenting into
/// `Presented` from `Update`.
fn app_with_events() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(TickedPlugin {
            source: TickSource::Manual,
            ..default()
        })
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 64.0,
        )))
        .init_resource::<Presented>()
        .register_ticked_component::<Position>()
        .add_ticked_event::<Thunk>()
        .add_systems(
            TickedSimulation,
            |tick: Res<CurrentTick>,
             positions: Query<&Position>,
             mut events: TickedEventWriter<Thunk>| {
                // Fires from *state*, so a corrected replay can legitimately
                // disagree with the prediction -- which is the case that matters.
                if let Some(position) = positions.iter().next() {
                    if position.0 >= 3 {
                        events.write(tick.0, Thunk(position.0 as u32));
                    }
                }
            },
        )
        .add_systems(
            Update,
            |mut events: TickedEventReader<Thunk>, mut seen: ResMut<Presented>| {
                seen.0.extend(events.read());
            },
        );
    app.world_mut().spawn((TickTrackedEntity(1), Position(0)));
    app
}

fn step(app: &mut App) {
    app.world_mut().write_message(StepForward);
    app.update();
}

#[test]
fn an_event_is_presented_once_however_many_times_its_tick_runs() {
    let mut app = app_with_events();
    step(&mut app); // tick 1, position 0 -- nothing

    // Move the body so the next tick fires.
    {
        let mut q = app.world_mut().query::<&mut Position>();
        let world = app.world_mut();
        for mut p in q.iter_mut(world) {
            p.0 = 5;
        }
    }
    step(&mut app); // tick 2 -- fires
    assert_eq!(
        app.world().resource::<Presented>().0,
        vec![(2, Thunk(5))],
        "the event should be presented once"
    );

    // Now replay tick 2 five times, as a client with a wobbly connection does.
    for _ in 0..5 {
        app.world_mut().write_message(ResetToTick(1));
        app.update();
        step(&mut app);
    }

    assert_eq!(
        app.world().resource::<Presented>().0,
        vec![(2, Thunk(5))],
        "a replayed tick must not re-announce what it already announced"
    );
}

#[test]
fn a_correction_presents_the_truth_and_not_the_prediction() {
    let mut app = app_with_events();
    {
        let mut q = app.world_mut().query::<&mut Position>();
        let world = app.world_mut();
        for mut p in q.iter_mut(world) {
            p.0 = 5;
        }
    }
    step(&mut app); // tick 1 predicts Thunk(5)
    assert_eq!(app.world().resource::<Presented>().0, vec![(1, Thunk(5))]);

    // The authority says the body was somewhere else. Roll back, correct, replay.
    app.world_mut().write_message(ResetToTick(0));
    app.update();
    {
        let mut q = app.world_mut().query::<&mut Position>();
        let world = app.world_mut();
        for mut p in q.iter_mut(world) {
            p.0 = 9;
        }
    }
    step(&mut app);

    assert_eq!(
        app.world().resource::<Presented>().0,
        vec![(1, Thunk(5)), (1, Thunk(9))],
        "the corrected tick must be presented; without the watermark rewind it \
         would be swallowed as already-shown and the player would only ever hear \
         the guess"
    );
}

#[test]
fn a_rewind_past_an_event_unpublishes_ticks_that_never_happened() {
    let mut app = app_with_events();
    {
        let mut q = app.world_mut().query::<&mut Position>();
        let world = app.world_mut();
        for mut p in q.iter_mut(world) {
            p.0 = 4;
        }
    }
    step(&mut app);
    step(&mut app);
    let before = app.world().resource::<Presented>().0.len();
    assert!(before >= 2, "two ticks should have fired, saw {before}");

    app.world_mut().write_message(ResetToTick(1));
    app.update();

    let events = app.world().resource::<TickedEvents<Thunk>>();
    assert!(
        events.at_tick(2).is_empty(),
        "tick 2 was rolled past, so its events are no longer part of the world"
    );
    assert!(!events.at_tick(1).is_empty(), "tick 1 still happened");
}
