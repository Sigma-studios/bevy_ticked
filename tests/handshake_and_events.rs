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

#[test]
fn the_wire_hash_notices_a_reorder() {
    // The failure this exists for is silent by construction: swapping two
    // registrations changes nothing that compiles, nothing that warns, and every
    // component read after the swap.
    let forward = registry(|app| {
        app.register_ticked_component::<Position>();
        app.register_ticked_component::<Velocity>();
    });
    let reversed = registry(|app| {
        app.register_ticked_component::<Velocity>();
        app.register_ticked_component::<Position>();
    });

    assert_ne!(
        forward.wire_hash(),
        reversed.wire_hash(),
        "the same types in a different order must not hash the same"
    );
}

#[test]
fn the_wire_hash_notices_an_addition() {
    let short = registry(|app| {
        app.register_ticked_component::<Position>();
    });
    let long = registry(|app| {
        app.register_ticked_component::<Position>();
        app.register_ticked_component::<Velocity>();
    });
    assert_ne!(short.wire_hash(), long.wire_hash());
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

#[test]
fn wire_names_are_in_registration_order() {
    let registry = registry(|app| {
        app.register_ticked_component::<Position>();
        app.register_ticked_component::<Velocity>();
    });
    let names: Vec<&str> = registry.wire_names().collect();
    assert_eq!(names.len(), 2);
    assert!(names[0].ends_with("Position"), "saw {names:?}");
    assert!(names[1].ends_with("Velocity"), "saw {names:?}");
    // And the index a consumer would assert against agrees with the position.
    assert_eq!(registry.index_of::<Position>(), Some(0));
    assert_eq!(registry.index_of::<Velocity>(), Some(1));
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
