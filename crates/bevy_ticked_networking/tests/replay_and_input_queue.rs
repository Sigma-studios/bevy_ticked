//! What a client costs, what it keeps, and what it refuses.
//!
//! Covers §3.4, §3.6, §3.7 and §3.10 of `shooting_ropes/docs/upstream-needs.md`.
//! All four are now fixed. §3.7's tests kept their subject when its answer changed: they measure
//! simulations per frame, which is the quantity the entry was ever about, and they now pin that a
//! correct prediction costs none and a wrong one still costs the full lead.

use std::collections::HashMap;
use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ticked::prelude::*;
use bevy_ticked::registry::TickedComponentRegistry;
use bevy_ticked::tracked_entity::TickTrackedEntity;
use bevy_ticked::world_actions::WorldActions;
use bevy_ticked_networking::client::{
    ClientTickBuffer, LocalClientPlayer, PredictionCheck, SnapshotApplied,
};
use bevy_ticked_networking::input::InputQueue;
use bevy_ticked_networking::messages::ReceivedNetworkSnapshot;
use bevy_ticked_networking::prelude::*;
use bevy_ticked_networking::snapshot::{WorldSnapshot, build_snapshot};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
struct Input {
    forward: bool,
}

#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Pos(i32);

/// How many times `TickedSimulation` has run, i.e. the real cost of a frame.
#[derive(Resource, Default)]
struct SimRuns(u32);

const LOCAL: u128 = 7;

/// A client that has already taken the role. Pass `false` for a peer that has not
/// — the §3.4 case.
fn client_with_role(role: bool) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(TickedPlugin {
            source: TickSource::FixedUpdate,
            ..default()
        })
        .add_plugins(TickedClientPlugin::<Input>::new())
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 64.0,
        )))
        .init_resource::<SimRuns>()
        .register_networked_ticked_component::<Pos>()
        .add_systems(TickedSimulation, |mut runs: ResMut<SimRuns>| runs.0 += 1);
    app.world_mut().spawn((TickTrackedEntity(1), Pos(0)));
    if role {
        app.insert_resource(LocalClientPlayer(LOCAL));
    }
    app
}

fn client() -> App {
    client_with_role(true)
}

/// A host-shaped snapshot for `tick`, holding exactly the state the client already
/// predicted -- so a comparison, if there were one, would find nothing to correct.
fn snapshot_matching(client: &mut App, tick: u64) -> WorldSnapshot {
    let registry = client.world().resource::<TickedComponentRegistry>().clone();
    registry.capture_all(client.world_mut(), tick);
    let mut snapshot = build_snapshot(client.world_mut(), tick);
    snapshot.input_margins = HashMap::from([(LOCAL, 2)]);
    snapshot
}

fn deliver(app: &mut App, tick: u64) {
    let snapshot = snapshot_matching(app, tick);
    app.world_mut().trigger(ReceivedNetworkSnapshot(snapshot));
}

/// Get past the initial sync: the first snapshot un-pauses and skips ahead by the
/// tick buffer.
///
/// The body is spawned *after* the first update on purpose. `reset_on_join` clears
/// the world when the client role is taken (§3.12), so a client only has entities
/// the host sent it -- and standing one up here is the closest a test gets to
/// "the first snapshot brought a body".
fn sync(app: &mut App) {
    app.update();
    app.world_mut().spawn((TickTrackedEntity(1), Pos(0)));
    deliver(app, 0);
    app.update();
}

fn applied(app: &mut App) -> Vec<SnapshotApplied> {
    app.world_mut()
        .resource_mut::<Messages<SnapshotApplied>>()
        .drain()
        .collect()
}

// ── §3.4 ─────────────────────────────────────────────────────────────────────

/// A peer that has not taken the client role must not have the host's world
/// written into it. The transport's data channel comes up before the lobby is
/// promoted, so this window is likely rather than merely possible.
#[test]
fn a_snapshot_arriving_before_the_client_role_is_ignored() {
    let mut app = client_with_role(false);
    app.update();

    let before = {
        let mut q = app.world_mut().query::<&Pos>();
        q.iter(app.world()).next().copied()
    };
    // A snapshot that would move the body, from a peer we have not agreed to follow.
    let mut snapshot = snapshot_matching(&mut app, 0);
    let index = app
        .world()
        .resource::<TickedComponentRegistry>()
        .index_of::<Pos>()
        .unwrap();
    snapshot.components.insert(
        index,
        HashMap::from([(1u64, postcard::to_allocvec(&Pos(999)).unwrap())]),
    );
    app.world_mut().trigger(ReceivedNetworkSnapshot(snapshot));
    app.update();

    let mut q = app.world_mut().query::<&Pos>();
    assert_eq!(
        q.iter(app.world()).next().copied(),
        before,
        "a peer that is not a client must not apply a snapshot"
    );
}

/// ...and it is *discarded*, not queued. A snapshot held until the role arrives
/// would be applied stale, which is worse than losing it -- and losing one costs
/// nothing, because they are unreliable by construction.
#[test]
fn an_ignored_snapshot_is_not_applied_later() {
    let mut app = client_with_role(false);
    app.update();

    let mut snapshot = snapshot_matching(&mut app, 0);
    let index = app
        .world()
        .resource::<TickedComponentRegistry>()
        .index_of::<Pos>()
        .unwrap();
    snapshot.components.insert(
        index,
        HashMap::from([(1u64, postcard::to_allocvec(&Pos(999)).unwrap())]),
    );
    app.world_mut().trigger(ReceivedNetworkSnapshot(snapshot));
    app.update();

    // The role arrives a moment later, as it does in a real join.
    app.insert_resource(LocalClientPlayer(LOCAL));
    app.update();
    app.update();

    let mut q = app.world_mut().query::<&Pos>();
    assert_ne!(
        q.iter(app.world()).next().copied(),
        Some(Pos(999)),
        "the stale snapshot must not surface once the role is taken"
    );
}

// ── §3.10 ────────────────────────────────────────────────────────────────────

#[test]
fn the_initial_sync_is_marked_and_later_corrections_are_not() {
    let mut app = client();
    app.update();

    deliver(&mut app, 0);
    app.update();
    let first = applied(&mut app);
    assert_eq!(first.len(), 1, "one snapshot applied, saw {}", first.len());
    assert!(first[0].first, "the initial sync must say so");

    let current = app.world().resource::<CurrentTick>().0;
    deliver(&mut app, current.saturating_sub(2));
    app.update();
    let later = applied(&mut app);
    assert_eq!(later.len(), 1);
    assert!(
        !later[0].first,
        "a steady-state correction is not an initial sync"
    );
}

// ── §3.6 ─────────────────────────────────────────────────────────────────────

#[test]
fn the_input_queue_is_pruned_to_the_history_window() {
    let mut app = client();
    sync(&mut app);
    app.insert_resource(HistoryBufferTicks(4));

    for _ in 0..40 {
        let tick = app.world().resource::<CurrentTick>().0;
        app.world_mut()
            .resource_mut::<InputQueue<Input>>()
            .insert(tick + 1, LOCAL, Input { forward: true });
        app.update();
    }

    let current = app.world().resource::<CurrentTick>().0;
    let queue = app.world().resource::<InputQueue<Input>>();
    let oldest = queue.inputs.keys().min().copied().unwrap();

    assert!(
        oldest >= current.saturating_sub(4),
        "tick {oldest} is older than the {current}-4 window and should have gone"
    );
    assert!(
        queue.inputs.len() <= 8,
        "the queue should track the window, not the session; holding {} ticks",
        queue.inputs.len()
    );
    // The tick that is still needed is still there.
    assert!(
        queue.get(current, LOCAL).is_some(),
        "pruning must not eat the current tick"
    );
}

/// The safety property behind the shared window: a replay never reaches further
/// back than the oldest tick that has component state to roll back to.
#[test]
fn pruning_inputs_on_the_history_window_cannot_starve_a_replay() {
    let mut app = client();
    app.insert_resource(HistoryBufferTicks(16));
    sync(&mut app);
    for _ in 0..40 {
        app.update();
    }

    let registry = app.world().resource::<TickedComponentRegistry>().clone();
    let current = app.world().resource::<CurrentTick>().0;
    let oldest_state = app
        .world()
        .resource::<WorldActions<Pos>>()
        .oldest_recorded_tick()
        .unwrap();

    assert!(
        current.saturating_sub(oldest_state) <= 17,
        "history is bounded to the retention window"
    );
    assert!(
        !registry.has_tick_captured(app.world(), oldest_state.saturating_sub(1)),
        "so an input for a tick older than the window is already unusable"
    );
    let queue = app.world().resource::<InputQueue<Input>>();
    assert!(
        queue.inputs.keys().min().copied().unwrap_or(0) <= oldest_state,
        "and everything a replay could still reach is retained"
    );
}

// ── §3.7, fixed: the cost, measured before and after ─────────────────────────

/// The measurement that used to say this cost seven simulations a frame.
///
/// It reported `lead=6 per_frame=[7, 7, 7, 7, 7, 7, 7, 7]` — the client replaying its whole
/// prediction lead against snapshots that were byte-identical to what it had already computed,
/// on every frame, for ever. With the prediction check it reports `[1, 1, 1, 1, 1, 1, 1, 1]`:
/// the ordinary forward tick and nothing else.
///
/// Kept pointing at the same quantity rather than deleted, because the number is the claim.
#[test]
fn a_client_that_predicted_correctly_does_not_replay_at_all() {
    let mut app = client();
    sync(&mut app);

    let lead = app.world().resource::<ClientTickBuffer>().target_ticks;
    assert!(lead >= 2, "the client is supposed to lead the server");

    let mut per_frame = Vec::new();
    for _ in 0..8 {
        let current = app.world().resource::<CurrentTick>().0;
        deliver(&mut app, current.saturating_sub(lead));
        app.world_mut().resource_mut::<SimRuns>().0 = 0;
        app.update();
        per_frame.push(app.world().resource::<SimRuns>().0);
    }

    println!("lead={lead} per_frame={per_frame:?}");
    assert!(
        per_frame.iter().all(|&runs| runs <= 1),
        "a snapshot that agrees with the prediction must not cost a replay. \
         lead {lead}, saw {per_frame:?}"
    );
    // And the state is still right — skipping the correction is only sound because there was
    // nothing to correct.
    let mut q = app.world_mut().query::<&Pos>();
    assert_eq!(q.iter(app.world()).next().copied(), Some(Pos(0)));
}

/// The other half, and the one that would make the optimisation a bug if it failed: a snapshot
/// that *disagrees* still costs a full replay, and still wins.
#[test]
fn a_client_that_predicted_wrongly_still_replays_its_whole_lead() {
    let mut app = client();
    sync(&mut app);

    let lead = app.world().resource::<ClientTickBuffer>().target_ticks;
    let current = app.world().resource::<CurrentTick>().0;
    let snapshot_tick = current.saturating_sub(lead);

    // Deliver a snapshot the client cannot have predicted.
    let mut snapshot = snapshot_matching(&mut app, snapshot_tick);
    for entities in snapshot.components.values_mut() {
        for bytes in entities.values_mut() {
            *bytes = postcard::to_allocvec(&Pos(99)).unwrap();
        }
    }
    app.world_mut().trigger(ReceivedNetworkSnapshot(snapshot));

    app.world_mut().resource_mut::<SimRuns>().0 = 0;
    app.update();
    let runs = app.world().resource::<SimRuns>().0;

    assert!(
        runs >= lead as u32,
        "a correction still replays the lead: lead {lead}, saw {runs}"
    );
    let mut q = app.world_mut().query::<&Pos>();
    assert_eq!(
        q.iter(app.world()).next().copied(),
        Some(Pos(99)),
        "and the correction actually landed"
    );
}

/// Turning the check off restores the old behaviour exactly, which is what makes it measurable.
#[test]
fn the_prediction_check_can_be_turned_off() {
    let mut app = client();
    app.insert_resource(PredictionCheck(false));
    sync(&mut app);

    let lead = app.world().resource::<ClientTickBuffer>().target_ticks;
    let current = app.world().resource::<CurrentTick>().0;
    deliver(&mut app, current.saturating_sub(lead));
    app.world_mut().resource_mut::<SimRuns>().0 = 0;
    app.update();

    assert!(
        app.world().resource::<SimRuns>().0 >= lead as u32,
        "with the check off, an agreeing snapshot replays the lead as it always did"
    );
}

/// §3.7's fix is feasible without new bookkeeping: the client's own prediction for
/// the snapshot's tick is already in the history when the snapshot lands.
#[test]
fn the_client_already_holds_what_a_comparison_would_need() {
    let mut app = client();
    sync(&mut app);

    let target = app.world().resource::<CurrentTick>().0.saturating_sub(2);
    assert!(
        app.world().resource::<WorldActions<Pos>>().at_tick(target).is_some(),
        "WorldActions<T>::at_tick(snapshot_tick) is the prediction to compare against"
    );
}
