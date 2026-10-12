//! `TickedPresentation` draws from the simulation after each tick, `Teleported` keeps a jump from
//! being blended, and the transform guard — on in every debug build — holds the simulation to its
//! side.

use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ticked::prelude::*;

const TICK: Duration = Duration::from_micros(15_625);

/// A simulated position, moved by the simulation and never by the renderer.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
struct Pos(f32);

/// Where the body goes next tick, and whether it walks or jumps there.
#[derive(Resource, Clone, Copy, Debug, Default)]
struct Plan {
    step: f32,
    jump_to: Option<f32>,
}

fn walk(
    tick: Res<CurrentTick>,
    mut plan: ResMut<Plan>,
    mut bodies: Query<(Entity, &mut Pos)>,
    mut commands: Commands,
) {
    for (entity, mut pos) in &mut bodies {
        match plan.jump_to.take() {
            Some(to) => {
                pos.0 = to;
                commands.entity(entity).insert(Teleported(tick.0));
            }
            None => pos.0 += plan.step,
        }
    }
}

fn present(mut bodies: Query<(&Pos, &mut Transform)>) {
    for (pos, mut transform) in &mut bodies {
        transform.translation.x = pos.0;
    }
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, TransformPlugin))
        .add_plugins(TickedPlugin {
            source: TickSource::Hz(64.0),
            ..default()
        })
        .add_plugins(TickedInterpolationPlugin)
        .insert_resource(TimeUpdateStrategy::ManualDuration(TICK))
        .init_resource::<Plan>()
        .add_systems(TickedSimulation, walk)
        .add_systems(TickedPresentation, present);
    app
}

fn spawn_body(app: &mut App) -> Entity {
    app.world_mut()
        .spawn((
            Pos(0.0),
            Transform::default(),
            TickedInterpolation::default(),
        ))
        .id()
}

fn halfway(app: &App, body: Entity) -> f32 {
    app.world()
        .get::<TickedInterpolation>(body)
        .unwrap()
        .sample(0.5)
        .unwrap()
        .translation
        .x
}

#[test]
fn the_presentation_runs_after_the_tick_and_before_the_record() {
    let mut app = app();
    let body = spawn_body(&mut app);
    app.world_mut().resource_mut::<Plan>().step = 10.0;
    for _ in 0..4 {
        app.update();
    }
    let pos = app.world().get::<Pos>(body).unwrap().0;
    let recorded = app
        .world()
        .get::<TickedInterpolation>(body)
        .unwrap()
        .current()
        .unwrap()
        .translation
        .x;
    assert_eq!(
        recorded, pos,
        "the pose recorded for this tick is the one built from this tick's state"
    );
    assert_eq!(
        halfway(&app, body),
        pos - 5.0,
        "and it blends from the tick before"
    );
}

#[test]
fn a_teleported_body_is_drawn_where_it_arrived() {
    let mut app = app();
    let body = spawn_body(&mut app);
    app.world_mut().resource_mut::<Plan>().step = 1.0;
    for _ in 0..4 {
        app.update();
    }
    app.world_mut().resource_mut::<Plan>().jump_to = Some(500.0);
    app.update();
    assert_eq!(
        halfway(&app, body),
        500.0,
        "a jump is shown as an arrival, not as a slide across the map"
    );
    app.update();
    assert_eq!(
        halfway(&app, body),
        500.5,
        "and the tick after it blends as usual"
    );
}

#[test]
fn a_walk_is_blended_without_a_stamp() {
    let mut app = app();
    let body = spawn_body(&mut app);
    app.world_mut().resource_mut::<Plan>().step = 100.0;
    for _ in 0..4 {
        app.update();
    }
    let pos = app.world().get::<Pos>(body).unwrap().0;
    assert_eq!(halfway(&app, body), pos - 50.0);
}

// ── the guard ────────────────────────────────────────────────────────────────

/// The guard is `TickedPlugin`'s, installed in any build with debug assertions — which a test
/// build is unless it is asked to be otherwise.
fn guarded() -> App {
    app()
}

#[test]
fn the_guard_lets_a_separated_game_run() {
    let mut app = guarded();
    spawn_body(&mut app);
    app.world_mut().resource_mut::<Plan>().step = 1.0;
    for _ in 0..8 {
        app.update();
    }
}

#[test]
fn the_guard_allows_a_transform_inserted_by_the_tick() {
    let mut app = guarded();
    app.add_systems(
        TickedSimulation,
        |mut commands: Commands, mut done: Local<bool>| {
            if !*done {
                *done = true;
                commands.spawn((Pos(3.0), Transform::from_xyz(3.0, 0.0, 0.0)));
            }
        },
    );
    for _ in 0..4 {
        app.update();
    }
}

fn move_transforms_in_the_tick(world: &mut World) {
    let mut bodies = world.query::<&mut Transform>();
    for mut transform in bodies.iter_mut(world) {
        transform.translation.y += 1.0;
    }
}

#[test]
#[should_panic(expected = "the simulation wrote a transform it did not create")]
fn the_guard_catches_a_write_in_the_tick() {
    let mut app = guarded();
    spawn_body(&mut app);
    // Exclusive, so the startup read check (which skips exclusive systems) lets it through and
    // the write is what is caught.
    app.add_systems(TickedSimulation, move_transforms_in_the_tick);
    for _ in 0..4 {
        app.update();
    }
}

fn read_transforms(bodies: Query<&Transform>) {
    for transform in &bodies {
        let _ = transform.translation;
    }
}

#[test]
#[should_panic(expected = "read_transforms")]
fn the_guard_catches_a_read_at_startup() {
    let mut app = guarded();
    app.add_systems(TickedSimulation, read_transforms);
    app.update();
}

#[test]
fn an_allowed_reader_is_not_reported() {
    let mut app = guarded();
    app.allow_transform_access(module_path!())
        .add_systems(TickedSimulation, read_transforms);
    for _ in 0..4 {
        app.update();
    }
}
