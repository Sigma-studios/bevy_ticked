//! What a client does with an entity it does not control, and what it does with a correction.

use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ticked::prelude::*;
use bevy_ticked::registry::TickedComponentRegistry;
use bevy_ticked::tracked_entity::TickTrackedEntity;
use bevy_ticked_networking::client::{ClientTickBuffer, LocalClientPlayer};
use bevy_ticked_networking::input::InputQueue;
use bevy_ticked_networking::messages::ReceivedNetworkSnapshot;
use bevy_ticked_networking::prelude::*;
use bevy_ticked_networking::smoothing::SmoothingOffset;
use bevy_ticked_networking::snapshot::{EntityRecord, FullBody, SnapshotPacket};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Input {
    dx: i32,
}

#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Pos(i32);

const LOCAL: u128 = 7;
const REMOTE: u128 = 9;
const TICK: Duration = Duration::from_micros(15_625);

/// `Pos += input.dx` for the owner's input, held; and `Transform.x = Pos` for the renderer.
fn integrate(
    tick: Res<CurrentTick>,
    queue: Res<InputQueue<Input>>,
    mut bodies: Query<(&Owner, &mut Pos, Option<&mut Transform>)>,
) {
    let inputs = queue.at_tick_or_last(tick.0);
    for (owner, mut pos, transform) in &mut bodies {
        if let Some(input) = inputs.get(&owner.0) {
            pos.0 += input.dx;
        }
        if let Some(mut transform) = transform {
            transform.translation.x = pos.0 as f32;
        }
    }
}

/// `Transform` is registered for rollback, so a correction that reaches back before the
/// transforms existed would strip them; let enough ticks pass that every snapshot in these
/// tests lands inside the transforms' history.
const WARM_UP: usize = 24;

fn warm_up(app: &mut App) {
    for _ in 0..WARM_UP {
        app.update();
    }
}

fn client() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, TransformPlugin))
        .add_plugins(TickedPlugin {
            source: TickSource::Hz(64.0),
            ..default()
        })
        .add_plugins(TickedClientPlugin::<Input>::new())
        .add_plugins((TickedInterpolationPlugin, TickedSmoothingPlugin))
        .insert_resource(TimeUpdateStrategy::ManualDuration(TICK))
        .register_networked_ticked_component::<Pos>("Pos")
        .register_ticked_component::<Transform>()
        .add_systems(TickedSimulation, integrate);
    app.insert_resource(LocalClientPlayer(LOCAL));
    app.update();
    app
}

fn wire<T: bevy_ticked::registry::TickedComponent>(app: &App) -> u16 {
    app.world()
        .resource::<TickedComponentRegistry>()
        .wire_index_of::<T>()
        .unwrap()
}

/// A body per player at `local`/`remote` positions.
fn body(app: &App, tick: u64, local: i32, remote: i32) -> SnapshotPacket {
    let (pos, owner) = (wire::<Pos>(app), wire::<Owner>(app));
    let mut body = FullBody::default();
    // "Pos" < "bevy_ticked::Owner": Pos goes first.
    body.put(EntityRecord::new(1).with(pos, &Pos(local)).with(owner, &Owner(LOCAL)));
    body.put(EntityRecord::new(2).with(pos, &Pos(remote)).with(owner, &Owner(REMOTE)));
    let mut packet = SnapshotPacket::full(tick, body);
    packet.your_margin = 2;
    packet
}

fn deliver(app: &mut App, packet: SnapshotPacket) {
    app.world_mut().trigger(ReceivedNetworkSnapshot(packet));
}

fn pos_of(app: &mut App, id: u64) -> Option<Pos> {
    let mut q = app.world_mut().query::<(&TickTrackedEntity, &Pos)>();
    q.iter(app.world()).find(|(t, _)| t.0 == id).map(|(_, p)| *p)
}

fn entity_of(app: &mut App, id: u64) -> Entity {
    let mut q = app.world_mut().query::<(Entity, &TickTrackedEntity)>();
    q.iter(app.world()).find(|(_, t)| t.0 == id).map(|(e, _)| e).unwrap()
}

fn sync(app: &mut App) {
    let packet = body(app, 0, 0, 0);
    deliver(app, packet);
    app.update();
}

// ── modes ────────────────────────────────────────────────────────────────────

#[test]
fn the_local_players_entity_is_predicted_and_everybody_elses_is_not() {
    let mut app = client();
    sync(&mut app);
    let mine = entity_of(&mut app, 1);
    let theirs = entity_of(&mut app, 2);
    assert_eq!(
        app.world().get::<ReplicationMode>(mine).copied(),
        Some(ReplicationMode::Predicted),
        "an entity with the local player's Owner is predicted"
    );
    assert_eq!(
        app.world().get::<ReplicationMode>(theirs),
        None,
        "everyone else's carries no marker, which means interpolated"
    );
}

#[test]
fn an_interpolated_entity_shows_the_authority_a_few_ticks_back() {
    let mut app = client();
    sync(&mut app);
    // Three snapshots, the remote body walking; the local one still.
    for (tick, remote) in [(1, 10), (2, 20), (3, 30)] {
        let packet = body(&app, tick, 0, remote);
        deliver(&mut app, packet);
        app.update();
    }
    let latest = app.world().resource::<AuthoritativeHistory>().newest_tick().unwrap();
    let delay = app.world().resource::<InterpolationDelay>().0;
    let history = app.world().resource::<AuthoritativeHistory>().clone();
    let (shown_tick, record) = history
        .newest_at_or_before(latest - delay, 2)
        .expect("a record at or before the display tick");
    let expected: Pos = record.first().unwrap();
    assert!(shown_tick <= latest - delay);
    assert_eq!(
        pos_of(&mut app, 2),
        Some(expected),
        "the remote body is exactly where the authority had it {delay} ticks before the newest \
         snapshot, not where a replay put it"
    );
}

#[test]
fn a_predicted_entity_is_not_touched_by_the_interpolation_restore() {
    let mut app = client();
    sync(&mut app);
    // The local player walks: its body must move with the prediction, not sit on the record.
    for _ in 0..8 {
        let tick = app.world().resource::<CurrentTick>().0;
        app.world_mut()
            .resource_mut::<InputQueue<Input>>()
            .insert(tick + 1, LOCAL, Input { dx: 1 });
        app.update();
    }
    assert!(
        pos_of(&mut app, 1).unwrap().0 >= 8,
        "the predicted body moved with the local input: {:?}",
        pos_of(&mut app, 1)
    );
    assert_eq!(pos_of(&mut app, 2), Some(Pos(0)), "the interpolated one stayed on the record");
}

#[test]
fn a_predicted_remote_body_holds_its_last_input_during_replay() {
    let mut app = client();
    sync(&mut app);
    let theirs = entity_of(&mut app, 2);
    app.world_mut()
        .entity_mut(theirs)
        .insert(ReplicationMode::Predicted);
    // The remote player's last known input arrived via a relay for one tick only.
    let tick = app.world().resource::<CurrentTick>().0;
    app.world_mut()
        .resource_mut::<InputQueue<Input>>()
        .insert(tick + 1, REMOTE, Input { dx: 2 });
    for _ in 0..5 {
        app.update();
    }
    assert_eq!(
        pos_of(&mut app, 2),
        Some(Pos(10)),
        "five ticks at the held input of 2: the key stays down until the player says otherwise"
    );
}

#[test]
fn get_or_last_holds_the_last_known_input() {
    let mut queue = InputQueue::<Input>::default();
    queue.insert(3, REMOTE, Input { dx: 4 });
    assert_eq!(queue.get_or_last(7, REMOTE), Some(&Input { dx: 4 }));
    assert_eq!(queue.get_or_last(3, REMOTE), Some(&Input { dx: 4 }));
    assert_eq!(queue.get_or_last(2, REMOTE), None);
    assert_eq!(queue.get_or_last(7, LOCAL), None);
    let all = queue.at_tick_or_last(9);
    assert_eq!(all.get(&REMOTE), Some(&Input { dx: 4 }));
    assert_eq!(all.len(), 1);
}

// ── smoothing ────────────────────────────────────────────────────────────────

fn with_transforms(app: &mut App) {
    for id in [1, 2] {
        let entity = entity_of(app, id);
        app.world_mut().entity_mut(entity).insert((
            Transform::default(),
            TickedInterpolation::default(),
            CorrectionSmoothing::default(),
        ));
    }
    let theirs = entity_of(app, 2);
    app.world_mut()
        .entity_mut(theirs)
        .insert(ReplicationMode::Predicted);
    warm_up(app);
}

fn shown_x(app: &mut App, id: u64) -> f32 {
    let entity = entity_of(app, id);
    app.world().get::<Transform>(entity).unwrap().translation.x
}

#[test]
fn correction_smoothing_never_touches_the_local_player() {
    let mut app = client();
    sync(&mut app);
    with_transforms(&mut app);
    let lead = app.world().resource::<ClientTickBuffer>().target_replay_distance;
    let current = app.world().resource::<CurrentTick>().0;
    // The authority disagrees with both predictions by a unit.
    let packet = body(&app, current - lead, 1, 1);
    deliver(&mut app, packet);
    app.update();

    let mine = entity_of(&mut app, 1);
    let theirs = entity_of(&mut app, 2);
    assert!(
        app.world().get::<SmoothingOffset>(mine).is_none(),
        "the local player's correction is felt, not hidden"
    );
    let offset = app
        .world()
        .get::<SmoothingOffset>(theirs)
        .expect("the predicted remote body's correction is smoothed");
    assert!(offset.translation.length() > 0.0);
    assert_eq!(app.world().resource::<CorrectionStats>().corrections, 1);
}

#[test]
fn a_small_correction_decays_and_never_snaps() {
    let mut app = client();
    sync(&mut app);
    with_transforms(&mut app);
    let lead = app.world().resource::<ClientTickBuffer>().target_replay_distance;
    let current = app.world().resource::<CurrentTick>().0;
    let packet = body(&app, current - lead, 0, 1);
    deliver(&mut app, packet);
    app.update();

    let theirs = entity_of(&mut app, 2);
    let first = app.world().get::<SmoothingOffset>(theirs).unwrap().translation.length();
    assert!(first > 0.5, "the whole unit is hidden on the first frame: {first}");
    let mut last = first;
    let mut frames = 0;
    while last > 1e-3 {
        app.update();
        frames += 1;
        let now = app.world().get::<SmoothingOffset>(theirs).unwrap().translation.length();
        assert!(now < last, "the offset grew from {last} to {now} on frame {frames}");
        last = now;
        assert!(frames < 128, "a unit of correction should be gone within two seconds");
    }
    // What the renderer saw on the way: the true position plus the shrinking offset.
    assert!((shown_x(&mut app, 2) - 1.0).abs() < 1e-2);
}

#[test]
fn a_correction_beyond_max_offset_is_shown_as_a_jump() {
    let mut app = client();
    sync(&mut app);
    with_transforms(&mut app);
    let lead = app.world().resource::<ClientTickBuffer>().target_replay_distance;
    let current = app.world().resource::<CurrentTick>().0;
    let packet = body(&app, current - lead, 0, 500);
    deliver(&mut app, packet);
    app.update();
    let stats = *app.world().resource::<CorrectionStats>();
    assert_eq!(stats.snapped, 1, "a teleport is meant to be seen");
    // The blend shows the previous tick at a frame boundary; one more tick and both states
    // are past the jump.
    app.update();
    assert!((shown_x(&mut app, 2) - 500.0).abs() < 1e-3, "{}", shown_x(&mut app, 2));
}

#[test]
fn the_simulation_never_sees_the_smoothing_offset() {
    #[derive(Resource, Default)]
    struct SeenInTick(Vec<f32>);
    let mut app = client();
    sync(&mut app);
    with_transforms(&mut app);
    app.init_resource::<SeenInTick>().add_systems(
        TickedSimulation,
        (|bodies: Query<(&TickTrackedEntity, &Transform, &Pos)>, mut seen: ResMut<SeenInTick>| {
            for (t, transform, pos) in &bodies {
                if t.0 == 2 {
                    seen.0.push(transform.translation.x - pos.0 as f32);
                }
            }
        })
        .after(integrate),
    );
    let lead = app.world().resource::<ClientTickBuffer>().target_replay_distance;
    let current = app.world().resource::<CurrentTick>().0;
    let packet = body(&app, current - lead, 0, 1);
    deliver(&mut app, packet);
    for _ in 0..10 {
        app.update();
    }
    let seen = &app.world().resource::<SeenInTick>().0;
    assert!(!seen.is_empty());
    assert!(
        seen.iter().all(|d| d.abs() < 1e-3),
        "inside a tick the transform is the simulation's, never the smoothed one: {seen:?}"
    );
}

#[test]
fn prediction_error_is_measured_at_the_snapshot_tick() {
    let mut app = client();
    measure_prediction::<Pos>(&mut app, |a, b| (a.0 - b.0).abs() as f32);
    sync(&mut app);
    for _ in 0..4 {
        app.update();
    }
    let lead = app.world().resource::<ClientTickBuffer>().target_replay_distance;
    let current = app.world().resource::<CurrentTick>().0;
    let packet = body(&app, current - lead, 3, 0);
    deliver(&mut app, packet);
    app.update();
    let error = *app.world().resource::<PredictionError>();
    assert_eq!(error.samples, 1);
    assert!(
        (error.last - 3.0).abs() < 1e-6,
        "the local body was predicted at 0 and the authority said 3: {error:?}"
    );
}

#[test]
fn a_snapshot_too_late_for_the_rollback_is_still_recorded_for_interpolation() {
    let mut app = client();
    sync(&mut app);
    for _ in 0..8 {
        app.update();
    }
    let newer = body(&app, 6, 0, 60);
    let late = body(&app, 4, 0, 40);
    deliver(&mut app, newer);
    deliver(&mut app, late);
    app.update();
    let history = app.world().resource::<AuthoritativeHistory>();
    assert!(history.has_tick(6));
    assert!(
        history.has_tick(4),
        "the late packet was dropped for the rollback and kept for the drawn path"
    );
    let stats = *app.world().resource::<bevy_ticked_networking::diagnostics::ReplayStats>();
    assert_eq!(stats.dropped_stale, 1);
}

#[test]
fn a_snapshot_superseded_before_it_was_applied_is_still_recorded() {
    let mut app = client();
    sync(&mut app);
    for _ in 0..8 {
        app.update();
    }
    // Three snapshots in one frame: only the newest is applied, none is lost to the drawn path.
    for (tick, remote) in [(4, 40), (5, 50), (6, 60)] {
        let packet = body(&app, tick, 0, remote);
        deliver(&mut app, packet);
    }
    app.update();
    let history = app.world().resource::<AuthoritativeHistory>();
    for tick in [4, 5, 6] {
        assert!(history.has_tick(tick), "tick {tick} is in the history");
    }
    let stats = *app.world().resource::<bevy_ticked_networking::diagnostics::ReplayStats>();
    assert_eq!(stats.snapshots_applied, 2, "the initial sync and the newest of the three");
}
