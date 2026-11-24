//! Rollback and replication for resources — `coop_zombies/docs/upstream-needs.md` T5.
//!
//! The entry these close is not "resources would be convenient". It is that every shared mutable
//! fact in a game had to be a component, so it needed an entity invented to live on, and both
//! consumers wrote the same house rule reminding everybody to do that.

use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ticked::prelude::*;
use bevy_ticked::registry::TickedComponentRegistry;
use bevy_ticked::resource_registry::ResourceActions;
use bevy_ticked::tracked_entity::TickTrackedEntity;

/// The shape this exists for: a fact about the world with no entity to belong to.
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq)]
struct Round(u32);

#[derive(Component, Clone, Copy, Debug, PartialEq)]
struct Height(i32);

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
        .insert_resource(Round(1))
        .register_ticked_component::<Height>()
        .register_ticked_resource::<Round>();
    app
}

fn step(app: &mut App) {
    app.world_mut().write_message(StepForward);
    app.update();
}

fn restore(app: &mut App, tick: Tick) {
    let registry = app.world().resource::<TickedComponentRegistry>().clone();
    registry.restore_all(app.world_mut(), tick);
}

#[test]
fn a_resource_rolls_back() {
    let mut app = app();
    app.world_mut().spawn((TickTrackedEntity(1), Height(0)));

    step(&mut app);
    step(&mut app);
    app.world_mut().insert_resource(Round(2));
    step(&mut app);
    app.world_mut().insert_resource(Round(3));
    step(&mut app);

    assert_eq!(app.world().resource::<Round>(), &Round(3));
    restore(&mut app, Tick(2));
    assert_eq!(
        app.world().resource::<Round>(),
        &Round(1),
        "the round is what it was at tick 2, not what it is now"
    );
}

/// The property that makes it usable at all: a resource rolls back on the *same* call that rolls
/// components back, so a rewind cannot land on a world where half the state moved.
#[test]
fn a_resource_and_a_component_roll_back_together() {
    let mut app = app();
    let entity = app
        .world_mut()
        .spawn((TickTrackedEntity(1), Height(0)))
        .id();

    step(&mut app);
    step(&mut app);

    app.world_mut().entity_mut(entity).insert(Height(50));
    app.world_mut().insert_resource(Round(9));
    step(&mut app);

    restore(&mut app, Tick(2));

    assert_eq!(app.world().entity(entity).get::<Height>(), Some(&Height(0)));
    assert_eq!(app.world().resource::<Round>(), &Round(1));
}

/// History is bounded and truncated the same way, or a rewind could reach a resource state whose
/// matching component state has already gone.
#[test]
fn resource_history_is_truncated_with_the_components() {
    let mut app = app();
    app.world_mut().spawn((TickTrackedEntity(1), Height(0)));
    for round in 1..=5u32 {
        app.world_mut().insert_resource(Round(round));
        step(&mut app);
    }

    let registry = app.world().resource::<TickedComponentRegistry>().clone();
    registry.truncate_all_after(app.world_mut(), Tick(2));

    let actions = app.world().resource::<ResourceActions<Round>>();
    assert!(actions.at_tick(Tick(2)).is_some(), "tick 2 is kept");
    assert!(
        actions.at_tick(Tick(4)).is_none(),
        "and everything after it is gone, as it is for components"
    );
}

/// A resource registered *after* a tick has already been captured has no saved value for it.
/// Restoring must leave what is there rather than guessing.
#[test]
fn restoring_a_tick_with_no_saved_resource_leaves_it_alone() {
    let mut app = app();
    app.world_mut().spawn((TickTrackedEntity(1), Height(0)));
    step(&mut app);

    app.world_mut().insert_resource(Round(7));
    restore(&mut app, Tick(9_999));

    assert_eq!(
        app.world().resource::<Round>(),
        &Round(7),
        "an unknown tick is a no-op, not a reset to Default"
    );
}

/// What a session's end does to a registered resource. A `Round(7)` that survived into the
/// next lobby was the entry every consumer with a round counter wrote a leave system for.
#[test]
fn reset_all_returns_every_registered_resource_to_default() {
    #[derive(Resource, Clone, Copy, Debug, Default, PartialEq)]
    struct Score(u32);

    let mut app = app();
    app.register_ticked_resource::<Score>();
    app.world_mut().spawn((TickTrackedEntity(1), Height(0)));
    app.world_mut().insert_resource(Round(7));
    step(&mut app);
    step(&mut app);
    assert!(
        app.world()
            .resource::<ResourceActions<Round>>()
            .newest_recorded_tick()
            .is_some(),
        "there is history to forget"
    );

    let resources = app
        .world()
        .resource::<bevy_ticked::resource_registry::TickedResourceRegistry>()
        .clone();
    resources.reset_all(app.world_mut());

    assert_eq!(
        *app.world().resource::<Round>(),
        Round::default(),
        "the previous session's round must not be the next lobby's"
    );
    assert!(
        app.world()
            .resource::<ResourceActions<Round>>()
            .newest_recorded_tick()
            .is_none(),
        "and its history went with it"
    );
    assert!(
        app.world().get_resource::<Score>().is_none(),
        "a registered resource the game never inserted is not inserted behind its back"
    );
}

/// State another library keeps in a resource that is not `Clone`: what [`TickedMirror`] is for.
#[derive(Resource, Default)]
struct Queue(Vec<u32>);

#[derive(Clone, Default)]
struct QueueMirror(Vec<u32>);

impl TickedMirror for QueueMirror {
    fn read(world: &World) -> Self {
        Self(world.resource::<Queue>().0.clone())
    }

    fn write(self, world: &mut World) {
        world.resource_mut::<Queue>().0 = self.0;
    }
}

/// The library reacting to a restore through an observer, the way avian queues a tombstoned
/// body's contacts: while components are put back, before the resources are.
fn queue_on_insert(insert: On<Insert<Height>>, heights: Query<&Height>, mut queue: ResMut<Queue>) {
    if let Ok(height) = heights.get(insert.entity) {
        queue.0.push(height.0 as u32 + 1000);
    }
}

fn mirror_app() -> App {
    let mut app = app();
    app.init_resource::<Queue>()
        .register_ticked_mirror::<QueueMirror>()
        .add_observer(queue_on_insert);
    app
}

#[test]
fn a_mirror_rolls_back_state_that_is_not_a_clone_resource() {
    let mut app = mirror_app();
    app.world_mut().spawn((TickTrackedEntity(1), Height(0)));
    app.world_mut().resource_mut::<Queue>().0.clear();

    step(&mut app);
    app.world_mut().resource_mut::<Queue>().0 = vec![7];
    step(&mut app);
    let tick = app.world().resource::<CurrentTick>().0;
    app.world_mut().resource_mut::<Queue>().0 = vec![8, 9];
    step(&mut app);

    restore(&mut app, tick);
    assert_eq!(app.world().resource::<Queue>().0, vec![7]);
}

#[test]
fn a_mirror_is_written_back_after_the_components_it_reacts_to() {
    let mut app = mirror_app();
    let entity = app
        .world_mut()
        .spawn((TickTrackedEntity(1), Height(0)))
        .id();
    app.world_mut().resource_mut::<Queue>().0.clear();

    step(&mut app);
    let tick = app.world().resource::<CurrentTick>().0;
    app.world_mut().entity_mut(entity).insert(Height(5));
    app.world_mut().resource_mut::<Queue>().0.clear();
    step(&mut app);

    // Restoring `Height` re-inserts it, and the observer queues something on the way; the
    // mirror is written last, so what the queue held at `tick` is what stands.
    restore(&mut app, tick);
    assert_eq!(app.world().get::<Height>(entity), Some(&Height(0)));
    assert_eq!(app.world().resource::<Queue>().0, Vec::<u32>::new());
}
