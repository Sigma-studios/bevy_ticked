//! No id is ever issued twice in a session.
//!
//! §3.12 of `shooting_ropes/docs/upstream-needs.md`. `reset_on_host` and
//! `reset_on_join` used to zero `TickTrackedEntityCounter` while entities minted
//! from the old counter were still standing, so the next `next()` handed out an id
//! that was already in use. Because `apply_snapshot` keys the whole world by id,
//! the symptom looked nothing like an id collision: a rope colliding with a player
//! had its components merged onto that player and no rope entity was ever created.

use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ticked::prelude::*;
use bevy_ticked::tracked_entity::{TickTrackedEntity, TickTrackedEntityCounter};
use bevy_ticked_networking::client::LocalClientPlayer;
use bevy_ticked_networking::prelude::*;
use bevy_ticked_networking::server::LocalServerPlayer;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
struct Input;

#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Pos(i32);

fn peer() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(TickedPlugin {
            source: TickSource::Hz(64.0),
            ..default()
        })
        .add_plugins(TickedServerPlugin::<Input>::new())
        .add_plugins(TickedClientPlugin::<Input>::new())
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 64.0,
        )))
        .register_networked_ticked_component::<Pos>();
    app
}

/// Mint entities the way a game does, through the counter.
fn spawn_tracked(app: &mut App, count: usize) -> Vec<u64> {
    let mut ids = Vec::new();
    for i in 0..count {
        let id = app.world_mut().resource_mut::<TickTrackedEntityCounter>().next();
        ids.push(id.0);
        app.world_mut().spawn((id, Pos(i as i32)));
    }
    ids
}

fn tracked_ids(app: &mut App) -> Vec<u64> {
    let mut q = app.world_mut().query::<&TickTrackedEntity>();
    let mut ids: Vec<u64> = q.iter(app.world()).map(|t| t.0).collect();
    ids.sort_unstable();
    ids
}

/// A solo player opening their world to friends keeps the world, and the counter
/// moves up to meet it rather than back to zero.
#[test]
fn hosting_never_reissues_an_id_that_is_already_in_use() {
    let mut app = peer();
    let before = spawn_tracked(&mut app, 3);
    assert_eq!(before, vec![1, 2, 3]);

    app.insert_resource(LocalServerPlayer(1));
    app.update();

    assert_eq!(
        tracked_ids(&mut app),
        vec![1, 2, 3],
        "a solo world must survive being opened to others"
    );
    let next = app.world_mut().resource_mut::<TickTrackedEntityCounter>().next();
    assert!(
        !before.contains(&next.0),
        "issued {} again, which is already in use -- apply_snapshot keys the world \
         by id, so two entities sharing one merge into whichever the client holds",
        next.0
    );
}

/// A joining client has no claim on whatever it built alone, and keeping it is what
/// makes the collision possible. Despawn, then zero.
#[test]
fn joining_clears_the_world_it_is_about_to_be_given() {
    let mut app = peer();
    spawn_tracked(&mut app, 3);

    app.insert_resource(LocalClientPlayer(2));
    app.update();

    assert!(
        tracked_ids(&mut app).is_empty(),
        "the host's world is about to replace this one entirely"
    );
    assert_eq!(
        app.world().resource::<TickTrackedEntityCounter>().0,
        0,
        "and with nothing left standing, zero is safe"
    );
}

/// The property both paths exist for, stated once: across a whole session --
/// solo, then hosting, then more spawns -- no id repeats.
#[test]
fn no_id_is_ever_issued_twice_in_a_session() {
    let mut app = peer();
    let mut issued = spawn_tracked(&mut app, 5);

    app.insert_resource(LocalServerPlayer(1));
    app.update();

    issued.extend(spawn_tracked(&mut app, 5));

    let mut sorted = issued.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        issued.len(),
        "duplicate ids issued across the session boundary: {issued:?}"
    );
}

/// The door out puts registered resources back to their defaults, not just their histories.
///
/// `clear_all` forgot what a resource *had been*; nothing forgot what it *was*, so a round
/// counter walked into the next lobby still saying "round 7", and every consumer wrote the
/// `reset_round_on_leave` system this makes unnecessary.
#[test]
fn reset_on_leave_resets_registered_resources() {
    #[derive(Resource, Clone, Copy, Debug, Default, PartialEq)]
    struct Round(u32);

    let mut app = peer();
    app.register_ticked_resource::<Round>();
    app.insert_resource(Round(7));
    app.insert_resource(LocalServerPlayer(1));
    app.update();
    assert_eq!(*app.world().resource::<Round>(), Round(7));

    app.world_mut().remove_resource::<LocalServerPlayer>();
    app.update();

    assert_eq!(
        *app.world().resource::<Round>(),
        Round::default(),
        "the round the last session ended on is not the one the next lobby starts in"
    );
}
