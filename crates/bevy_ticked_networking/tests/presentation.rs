//! A game whose simulation never touches `Transform`: the position is simulated, and what is
//! drawn is built from it in `TickedPresentation`. The correction smoothing has to keep working.

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

/// The simulation: `Pos += input.dx`, and nothing about how it is drawn.
fn integrate(
    tick: Res<CurrentTick>,
    queue: Res<InputQueue<Input>>,
    mut bodies: Query<(&Owner, &mut Pos)>,
) {
    let inputs = queue.at_tick_or_last(tick.0);
    for (owner, mut pos) in &mut bodies {
        if let Some(input) = inputs.get(&owner.0) {
            pos.0 += input.dx;
        }
    }
}

/// The presentation: `Transform.x = Pos`, after the tick.
fn present(mut bodies: Query<(&Pos, &mut Transform)>) {
    for (pos, mut transform) in &mut bodies {
        transform.translation.x = pos.0 as f32;
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
        .add_systems(TickedSimulation, integrate)
        .add_systems(TickedPresentation, present);
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

fn body(app: &App, tick: Tick, local: i32, remote: i32) -> SnapshotPacket {
    let (pos, owner) = (wire::<Pos>(app), wire::<Owner>(app));
    let mut body = FullBody::default();
    body.put(
        EntityRecord::new(1)
            .with(pos, &Pos(local))
            .with(owner, &Owner(LOCAL)),
    );
    body.put(
        EntityRecord::new(2)
            .with(pos, &Pos(remote))
            .with(owner, &Owner(REMOTE)),
    );
    let mut packet = SnapshotPacket::full(tick, body);
    packet.your_margin = 2;
    packet
}

fn deliver(app: &mut App, packet: SnapshotPacket) {
    app.world_mut().trigger(ReceivedNetworkSnapshot(packet));
}

fn entity_of(app: &mut App, id: u64) -> Entity {
    let mut q = app.world_mut().query::<(Entity, &TickTrackedEntity)>();
    q.iter(app.world())
        .find(|(_, t)| t.0 == id)
        .map(|(e, _)| e)
        .unwrap()
}

/// Synced, both bodies drawn and smoothed, the remote one predicted.
fn ready() -> App {
    let mut app = client();
    let packet = body(&app, Tick::ZERO, 0, 0);
    deliver(&mut app, packet);
    app.update();
    for id in [1, 2] {
        let entity = entity_of(&mut app, id);
        app.world_mut().entity_mut(entity).insert((
            Transform::default(),
            TickedInterpolation::default(),
            CorrectionSmoothing::default(),
        ));
    }
    let theirs = entity_of(&mut app, 2);
    app.world_mut()
        .entity_mut(theirs)
        .insert(ReplicationMode::Predicted);
    for _ in 0..8 {
        app.update();
    }
    app
}

#[test]
fn a_correction_to_a_presented_body_is_smoothed() {
    let mut app = ready();
    let lead = app
        .world()
        .resource::<ClientTickBuffer>()
        .target_replay_distance;
    let current = app.world().resource::<CurrentTick>().0;
    // The authority puts the remote body a unit from where it was predicted.
    let packet = body(&app, current - lead, 0, 1);
    deliver(&mut app, packet);
    app.update();

    let theirs = entity_of(&mut app, 2);
    let offset = app.world().get::<SmoothingOffset>(theirs).expect(
        "the replay moved `Pos`, and the transform built from it after the replay is what \
             the smoothing measured",
    );
    assert!(offset.translation.length() > 0.5, "{offset:?}");
    let stats = *app.world().resource::<CorrectionStats>();
    assert_eq!(stats.corrections, 1);
    assert_eq!(stats.snapped, 0);
}
