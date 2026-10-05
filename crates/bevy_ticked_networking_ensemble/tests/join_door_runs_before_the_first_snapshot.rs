//! The join door runs before the first snapshot can be applied, every time.
//!
//! The doors used to be systems in `Update`, keyed on `resource_added::<LocalClientPlayer>` and
//! unordered against the bridge's `adopt_role`, which inserts the role through commands from
//! `Update` too. `RunTickedLoop` runs before `Update`, so a join whose door was evaluated before
//! the role's commands landed only ran it in the *next* frame's `Update` — after that frame's
//! `PreUpdate` had forwarded a snapshot (the gate is `RegistryVerified` plus the role, both
//! present) and its tick loop had applied it: onto the solo world, with `AwaitingSync` never
//! held. Measured on the old shape: forty joins out of forty.
//!
//! The doors are `OnEnter`/`OnExit` of `TickedSession` now, in `StateTransition`, which runs
//! after `PreUpdate` and before the tick loop. This is the measurement, kept as the regression.
//!
//! The probe is a system at the front of the client's snapshot set: it records, whenever a
//! snapshot is about to be applied, whether `AwaitingSync` is held. Only the join door holds it,
//! and only the first snapshot releases it, so "about to apply, not held, first snapshot of the
//! session" is exactly "applied before the join door ran".

use bevy::ecs::schedule::SingleThreadedExecutor;
use bevy::prelude::*;
use bevy_ensemble::{Lobby, ReceivedEnsembleMessage};
use bevy_ticked::TickedLoop;
use bevy_ticked::prelude::*;
use bevy_ticked_networking::client::{
    AppliedSnapshotTick, ClientSet, LocalClientPlayer, PendingSnapshotTick,
};
use bevy_ticked_networking::snapshot::{FullBody, SnapshotBody, SnapshotPacket, encode_packet};
use bevy_ticked_networking_ensemble::handshake::RegistryVerified;
use bevy_ticked_networking_ensemble::{EnsembleSnapshotMessage, StartSolo};
use bevy_ticked_testing::fixtures::minimal::{self, Input};
use bevy_ticked_testing::prelude::*;

#[derive(Resource, Default, Debug)]
struct Probe {
    frame: u32,
    /// Frame the role first appeared (observed in `Last`).
    role_at: Option<u32>,
    /// Frame `AwaitingSync` was first seen held (observed in `Last`): the join door ran.
    reset_at: Option<u32>,
    /// Every snapshot about to be applied: (frame, awaiting_sync_held, current_tick before).
    applies: Vec<(u32, bool, Tick)>,
}

fn before_apply(
    probe: Option<ResMut<Probe>>,
    pending: Res<PendingSnapshotTick>,
    holds: Res<TickHolds>,
    tick: Res<CurrentTick>,
    client: Option<Res<LocalClientPlayer>>,
) {
    let Some(mut probe) = probe else { return };
    if pending.0.is_some() && client.is_some() {
        let frame = probe.frame;
        probe
            .applies
            .push((frame, holds.holds(TickHoldReason::AwaitingSync), tick.0));
    }
}

fn end_of_frame(
    mut probe: ResMut<Probe>,
    holds: Res<TickHolds>,
    client: Option<Res<LocalClientPlayer>>,
) {
    let frame = probe.frame;
    if client.is_some() && probe.role_at.is_none() {
        probe.role_at = Some(frame);
    }
    if holds.holds(TickHoldReason::AwaitingSync) && probe.reset_at.is_none() {
        probe.reset_at = Some(frame);
    }
    probe.frame += 1;
}

fn snapshot(tick: Tick) -> ReceivedEnsembleMessage<EnsembleSnapshotMessage> {
    ReceivedEnsembleMessage {
        sender: Some(HOST_UUID),
        message: EnsembleSnapshotMessage {
            bytes: encode_packet(&SnapshotPacket {
                seq: tick.0 as u32,
                tick,
                your_margin: 0,
                body: SnapshotBody::Full(FullBody::default()),
            }),
        },
        received_at: bevy_ensemble::Instant::now(),
    }
}

/// One join. Returns the probe: did a snapshot get applied before the join door?
fn one_join(single_threaded: bool, solo: bool) -> Probe {
    let mut app = client_server_peer::<Input>(2, |app| {
        minimal::install(app);
        app.init_resource::<Probe>()
            .add_systems(TickedLoop, before_apply.in_set(ClientSet::BeforeSnapshot))
            .add_systems(Last, end_of_frame);
        if single_threaded {
            app.edit_schedule(Update, |s| {
                s.set_executor(SingleThreadedExecutor::new());
            });
        }
    });
    // A world that has been running a while: offline, or a solo session.
    if solo {
        app.world_mut().write_message(StartSolo);
    }
    for _ in 0..30 {
        app.update();
    }
    // The host's registries have already been compared (`check_registry` does not wait for a
    // role), and the backend has promoted the joined lobby. Snapshots are flowing every frame.
    app.insert_resource(RegistryVerified);
    app.world_mut().spawn(Lobby);
    for i in 0..6 {
        app.world_mut().write_message(snapshot(Tick(1000 + i)));
        app.update();
    }
    let mut probe = std::mem::take(&mut *app.world_mut().resource_mut::<Probe>());
    // Frames are counted from the start; make them relative to the join for reading.
    let base = 30;
    probe.role_at = probe.role_at.map(|f| f - base);
    probe.reset_at = probe.reset_at.map(|f| f - base);
    for a in &mut probe.applies {
        a.0 -= base;
    }
    let applied = app.world().resource::<AppliedSnapshotTick>().0;
    // Zero out of N means nothing if nothing was applied at all.
    assert!(
        applied.is_some() && !probe.applies.is_empty(),
        "the join never applied a snapshot: {probe:?}"
    );
    println!(
        "role at frame {:?}, join door at frame {:?}, applies (frame, AwaitingSync held, tick \
         before) {:?}, applied tick now {:?}",
        probe.role_at, probe.reset_at, probe.applies, applied
    );
    probe
}

fn applied_before_reset(probe: &Probe) -> bool {
    probe
        .applies
        .first()
        .is_some_and(|(_, awaiting_sync, _)| !awaiting_sync)
}

/// Many fresh apps, multi-threaded executor (what ships): count how often the first snapshot
/// lands on the solo world.
#[test]
fn no_snapshot_is_applied_before_the_join_door_multi_threaded() {
    let runs = 40;
    let bad = (0..runs)
        .filter(|_| applied_before_reset(&one_join(false, false)))
        .count();
    println!("multi-threaded: {bad}/{runs} joins applied a snapshot before the join door");
    assert_eq!(
        bad, 0,
        "{bad}/{runs} joins applied a snapshot onto the solo world"
    );
}

/// The same with a single-threaded `Update`: insertion order decides.
#[test]
fn no_snapshot_is_applied_before_the_join_door_single_threaded() {
    let runs = 5;
    let bad = (0..runs)
        .filter(|_| applied_before_reset(&one_join(true, false)))
        .count();
    println!("single-threaded: {bad}/{runs} joins applied a snapshot before the join door");
    assert_eq!(
        bad, 0,
        "{bad}/{runs} joins applied a snapshot onto the solo world"
    );
}

/// From a solo session: the lobby appears under it, the world is handed over, and the client role
/// replaces the solo one in a single door.
#[test]
fn no_snapshot_is_applied_before_the_join_door_from_solo() {
    let runs = 20;
    let bad = (0..runs)
        .filter(|_| applied_before_reset(&one_join(false, true)))
        .count();
    println!("from solo: {bad}/{runs} joins applied a snapshot before the join door");
    assert_eq!(
        bad, 0,
        "{bad}/{runs} joins applied a snapshot onto the solo world"
    );
}
