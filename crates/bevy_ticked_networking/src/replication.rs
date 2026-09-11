//! What a client does with an entity it does not control.
//!
//! # The failure this exists for
//!
//! A client used to simulate every tracked entity through its replay with whatever input it
//! had, which for a remote player was nothing: a body that was walking on the host stood still
//! for the whole prediction lead on every client, then snapped to where the next snapshot said
//! it was, sixty-four times a second. It read as remote players stuttering, and every game that
//! shipped on this stack wrote a smoothing layer over it.
//!
//! # Two modes
//!
//! [`ReplicationMode::Predicted`] is for the entities the local player drives: simulated through
//! the replay from the player's own inputs, corrected by snapshots. Everything else is
//! [`ReplicationMode::Interpolated`] by default: shown at the authority's state a few ticks in
//! the past, blending between the two authoritative ticks around that moment, never predicted.
//! A little behind, always smooth, and exactly where the host said.
//!
//! The mode is a client-side component and is not sent. An entity with no marker is
//! interpolated. [`Owner`] is the networked component that says whose an entity is, and
//! [`RemoteInterpolationPlugin`] marks the local player's as predicted on its own.
//!
//! # How an interpolated entity is drawn
//!
//! Every tick, after the snapshot and its replay, each interpolated entity's networked
//! components are set to the authoritative record for `latest applied tick - InterpolationDelay`
//! (the newest one at or before it). `TickedInterpolation` then sees one authoritative state per
//! tick and blends between them for the renderer, as it does for anything else. The record comes
//! from [`AuthoritativeHistory`], which keeps the last few snapshots' records verbatim.
//!
//! The simulation still runs on interpolated entities between restores, so a physics body should
//! be kinematic on a client (the example does that in an observer): the host owns its motion,
//! and a dynamic body would fight the restore every tick.

use std::collections::BTreeMap;

use bevy::prelude::*;
use bevy_ticked::{
    TickedLoop, registry::TickedComponentRegistry, tracked_entity::TickTrackedEntity,
};
use serde::{Deserialize, Serialize};

use crate::client::{AppliedSnapshotTick, ClientSet, LocalClientPlayer};
use crate::snapshot::EntityRecord;

/// How a client treats a tracked entity. Absent means [`Interpolated`](Self::Interpolated).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ReplicationMode {
    /// Simulated through the replay and corrected by snapshots: the local player's entities.
    Predicted,
    /// Shown at the authority's state a few ticks in the past, never predicted.
    #[default]
    Interpolated,
}

/// Which player an entity belongs to. Networked under `"bevy_ticked::Owner"`, registered by
/// both role plugins.
///
/// Every game had one (`OwnerPlayer`, `PlayerUuid`, `Owner`), and every game used it for the
/// same three things: which body gets the camera, which body is predicted, which body a
/// snapshot's absence rule may not touch. Upstream, so the stack can answer the second.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Owner(pub u128);

/// How far behind the newest applied snapshot an interpolated entity is shown, in ticks.
///
/// Two ticks by default: enough that a lost snapshot leaves a state to blend toward, small
/// enough that a remote body is where it was thirty milliseconds ago. Raise it on a lossy
/// link, or when the host sends less often than every tick (the send-rate phase sets it to
/// twice `send_every`).
#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq)]
pub struct InterpolationDelay(pub u64);

impl Default for InterpolationDelay {
    fn default() -> Self {
        Self(2)
    }
}

/// How far behind its target the display tick may fall before it snaps rather than crawls.
const CATCH_UP_SNAP: u64 = 16;

/// The tick an interpolated entity is currently shown at.
///
/// It advances one tick per tick toward `latest applied - InterpolationDelay`, never faster:
/// snapshots arrive in bunches on a jittery link, and a display tick that followed the
/// applied tick one for one moved a remote body by three ticks in one frame and none in the
/// next. Only a clock more than [`CATCH_UP_SNAP`] ticks behind snaps.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DisplayTick(pub Option<u64>);

/// The last few snapshots' entity records, verbatim, by tick then id.
///
/// What an interpolated entity is drawn from, what the misprediction comparison reads, and
/// what a delta is built against. Bounded to [`max_ticks`](Self::max_ticks); the oldest goes
/// when a newer arrives.
#[derive(Resource, Debug, Clone)]
pub struct AuthoritativeHistory {
    ticks: BTreeMap<u64, BTreeMap<u64, EntityRecord>>,
    /// Whole bodies by packet `seq`: what a delta is rebuilt against.
    bodies: BTreeMap<u32, (u64, crate::snapshot::FullBody)>,
    /// How many snapshot ticks to keep.
    pub max_ticks: usize,
}

impl Default for AuthoritativeHistory {
    fn default() -> Self {
        Self {
            ticks: BTreeMap::new(),
            bodies: BTreeMap::new(),
            max_ticks: 64,
        }
    }
}

impl AuthoritativeHistory {
    /// Remember a snapshot's records at `tick`.
    pub fn record(&mut self, tick: u64, records: impl IntoIterator<Item = EntityRecord>) {
        let by_id: BTreeMap<u64, EntityRecord> = records
            .into_iter()
            .map(|record| (record.id, record))
            .collect();
        self.ticks.insert(tick, by_id);
        while self.ticks.len() > self.max_ticks {
            self.ticks.pop_first();
        }
    }

    /// Remember a whole body under the packet `seq` that carried it, for deltas against it.
    pub fn record_body(&mut self, seq: u32, tick: u64, body: &crate::snapshot::FullBody) {
        self.bodies.insert(seq, (tick, body.clone()));
        while self.bodies.len() > self.max_ticks {
            self.bodies.pop_first();
        }
    }

    /// The body the packet `seq` carried, if still held.
    pub fn body_at_seq(&self, seq: u32) -> Option<&crate::snapshot::FullBody> {
        self.bodies.get(&seq).map(|(_, body)| body)
    }

    /// The record for `id` at the newest tick at or before `tick`, with that tick.
    pub fn newest_at_or_before(&self, tick: u64, id: u64) -> Option<(u64, &EntityRecord)> {
        self.ticks
            .range(..=tick)
            .rev()
            .find_map(|(at, records)| records.get(&id).map(|record| (*at, record)))
    }

    /// Whether a snapshot for `tick` is held.
    pub fn has_tick(&self, tick: u64) -> bool {
        self.ticks.contains_key(&tick)
    }

    /// The record for `id` at exactly `tick`.
    pub fn at(&self, tick: u64, id: u64) -> Option<&EntityRecord> {
        self.ticks.get(&tick)?.get(&id)
    }

    /// Every id the snapshot at `tick` named.
    pub fn ids_at(&self, tick: u64) -> impl Iterator<Item = u64> + '_ {
        self.ticks
            .get(&tick)
            .into_iter()
            .flat_map(|r| r.keys().copied())
    }

    /// The newest tick held.
    pub fn newest_tick(&self) -> Option<u64> {
        self.ticks.keys().next_back().copied()
    }

    /// The oldest tick held.
    pub fn oldest_tick(&self) -> Option<u64> {
        self.ticks.keys().next().copied()
    }

    pub fn clear(&mut self) {
        self.ticks.clear();
        self.bodies.clear();
    }
}

/// Register [`Owner`] on the wire, once, whichever role plugin comes first.
pub(crate) fn install_owner(app: &mut App) {
    use crate::networked_registry::NetworkedTickedAppExt;
    let registered = app
        .world()
        .get_resource::<TickedComponentRegistry>()
        .is_some_and(|registry| registry.index_of::<Owner>().is_some());
    if !registered {
        app.register_networked_ticked_component_once::<Owner>("bevy_ticked::Owner");
    }
    // The allocator is rolled back by the core; on the wire, the authority's snapshot corrects
    // a client's counters (slot 0 above all: the ids the host has handed out).
    use crate::networked_registry::NetworkedTickedResourceAppExt;
    let allocator_networked = app
        .world()
        .get_resource::<bevy_ticked::resource_registry::TickedResourceRegistry>()
        .is_some_and(|registry| {
            registry
                .index_of::<bevy_ticked::tracked_entity::TrackedIdAllocator>()
                .is_some()
                && registry.wire_names_unfrozen_contains("bevy_ticked::TrackedIdAllocator")
        });
    if !allocator_networked {
        app.register_networked_ticked_resource::<bevy_ticked::tracked_entity::TrackedIdAllocator>(
            "bevy_ticked::TrackedIdAllocator",
        );
    }
}

/// Shows every interpolated entity at the authority's state, a few ticks back.
///
/// Installed by `TickedClientPlugin`. Marks the local player's entities as predicted from
/// [`Owner`], and restores every other tracked entity to its authoritative record after each
/// snapshot's replay.
pub struct RemoteInterpolationPlugin;

impl Plugin for RemoteInterpolationPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<InterpolationDelay>()
            .init_resource::<AuthoritativeHistory>()
            .init_resource::<DisplayTick>()
            .add_observer(mark_owned_on_add)
            .add_systems(
                Update,
                mark_owned_on_role.run_if(resource_exists_and_changed::<LocalClientPlayer>),
            )
            .add_systems(
                TickedLoop,
                restore_interpolated_entities.in_set(ClientSet::AfterSnapshot),
            );
    }
}

/// An entity that appears with the local player's `Owner` is predicted; one that appears with
/// anybody else's keeps the default.
fn mark_owned_on_add(
    add: On<Add, Owner>,
    local: Option<Res<LocalClientPlayer>>,
    owners: Query<&Owner>,
    mut commands: Commands,
) {
    let Some(local) = local else { return };
    if owners.get(add.entity).is_ok_and(|owner| owner.0 == local.0) {
        commands
            .entity(add.entity)
            .try_insert(ReplicationMode::Predicted);
    }
}

/// When the role arrives after the entities did (a solo world that becomes a client), mark
/// what the local player already owned, and un-mark what it does not.
fn mark_owned_on_role(
    local: Option<Res<LocalClientPlayer>>,
    owned: Query<(Entity, &Owner, Option<&ReplicationMode>)>,
    mut commands: Commands,
) {
    let Some(local) = local else { return };
    for (entity, owner, mode) in &owned {
        let mine = owner.0 == local.0;
        match (mine, mode) {
            (true, Some(ReplicationMode::Predicted)) => {}
            (true, _) => {
                commands
                    .entity(entity)
                    .try_insert(ReplicationMode::Predicted);
            }
            (false, Some(ReplicationMode::Predicted)) => {
                commands.entity(entity).try_remove::<ReplicationMode>();
            }
            (false, _) => {}
        }
    }
}

/// After the snapshot and its replay, put every interpolated entity at the authority's state
/// for `latest - delay`.
///
/// Runs every pass of the loop, replay or not: a tick that ran with no snapshot still moved
/// the entity by simulation, and the renderer must see the authoritative state again.
fn restore_interpolated_entities(world: &mut World) {
    if !world.contains_resource::<LocalClientPlayer>() {
        return;
    }
    let Some(latest) = world.resource::<AppliedSnapshotTick>().0 else {
        return;
    };
    let target = latest.saturating_sub(world.resource::<InterpolationDelay>().0);
    let display_tick = {
        let mut display = world.resource_mut::<DisplayTick>();
        let next = match display.0 {
            // One tick per tick, whatever arrived: a bunch of late snapshots is absorbed over
            // as many frames rather than shown as one jump. A clock that has fallen far behind
            // (a stall, a pause) snaps instead of crawling for seconds.
            Some(shown) if shown + CATCH_UP_SNAP < target => target.saturating_sub(2),
            Some(shown) => (shown + 1).min(target),
            None => target.saturating_sub(2),
        };
        display.0 = Some(next);
        next
    };
    let registry = world.resource::<TickedComponentRegistry>().clone();

    let targets: Vec<(Entity, u64)> = {
        let mut query = world.query::<(Entity, &TickTrackedEntity, Option<&ReplicationMode>)>();
        query
            .iter(world)
            .filter(|(_, _, mode)| !matches!(mode, Some(ReplicationMode::Predicted)))
            .map(|(entity, tracked, _)| (entity, tracked.0))
            .collect()
    };
    if targets.is_empty() {
        return;
    }
    let records: Vec<(Entity, EntityRecord)> = {
        let history = world.resource::<AuthoritativeHistory>();
        targets
            .iter()
            .filter_map(|(entity, id)| {
                history
                    .newest_at_or_before(display_tick, *id)
                    .map(|(_, record)| (*entity, record.clone()))
            })
            .collect()
    };
    for (entity, record) in records {
        let mut rest: &[u8] = &record.bytes;
        for wire_index in record.present.iter() {
            match registry.insert_one(world, wire_index, entity, rest) {
                Some(consumed) => rest = &rest[consumed..],
                None => break,
            }
        }
        registry.remove_absent(world, entity, &record.present);
    }
}
