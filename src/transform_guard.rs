//! Opt-in checks that the simulation leaves `Transform` to the presentation.
//!
//! A game that draws from [`TickedPresentation`](crate::TickedPresentation) has one rule for its
//! simulation: it neither reads nor writes `Transform` or `GlobalTransform`. Positions live in
//! simulated components, and what is drawn is built from them after the tick. The rule is easy
//! to break without noticing — a system that writes a transform "for the renderer" works, and
//! only puts the picture a tick behind whatever moves the body afterwards, or a blended value in
//! front of the next tick — so [`TransformGuardPlugin`] turns each half of it into a panic:
//!
//! - **writes**, checked around every run of [`TickedSimulation`](crate::TickedSimulation),
//!   replays included: a `Transform` or `GlobalTransform` that changed during the run and was not
//!   inserted during it. Inserting one is allowed — a body spawned by the tick gets its first
//!   transform from whoever draws it, through an observer that runs inside the tick.
//! - **reads**, checked once at startup, from the access of every system in
//!   `TickedSimulation`. A dependency whose systems declare a transform they leave unused — a
//!   physics engine with its sync switched off — is allowed by name with
//!   [`TransformGuardAppExt::allow_transform_access`]. Exclusive systems are skipped: they can
//!   reach anything, and so say nothing.
//!
//! Neither check is free and neither is needed in a shipped build. A game turns the plugin on
//! for its development builds and tests.

use bevy::ecs::change_detection::Tick as ChangeTick;
use bevy::ecs::schedule::Schedules;
use bevy::ecs::system::System;
use bevy::prelude::*;

use crate::TickedSimulation;

/// Panic when the simulation reads or writes `Transform`. See the [module docs](self).
pub struct TransformGuardPlugin;

/// The guard's state: which systems may declare a transform, and whether the read check ran.
#[derive(Resource, Debug, Default)]
pub struct TransformGuard {
    /// Prefixes of system names allowed to declare `Transform` access inside the simulation.
    allowed: Vec<String>,
    audited: bool,
}

impl Plugin for TransformGuardPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<TransformGuard>()
            .add_systems(PreStartup, audit_simulation_reads);
    }
}

/// Allow a dependency's systems to declare transform access inside the simulation.
pub trait TransformGuardAppExt {
    /// Systems whose name starts with `prefix` are not reported by the read check — a crate's
    /// path, typically (`"avian2d::"`). Harmless without [`TransformGuardPlugin`].
    fn allow_transform_access(&mut self, prefix: impl Into<String>) -> &mut Self;
}

impl TransformGuardAppExt for App {
    fn allow_transform_access(&mut self, prefix: impl Into<String>) -> &mut Self {
        self.world_mut()
            .get_resource_or_insert_with(TransformGuard::default)
            .allowed
            .push(prefix.into());
        self
    }
}

/// The world's change tick before a run of the simulation, if the guard is on.
pub(crate) fn begin(world: &mut World) -> Option<ChangeTick> {
    if !world.contains_resource::<TransformGuard>() {
        return None;
    }
    // A system's run is stamped with the tick *before* the increment it makes, so the first
    // system of the run would share this tick and its writes would not count as newer.
    let started = world.change_tick();
    world.increment_change_tick();
    Some(started)
}

/// After a run of the simulation: panic if it moved a transform it did not create.
pub(crate) fn end(world: &mut World, started: Option<ChangeTick>) {
    let Some(started) = started else {
        return;
    };
    if !world.contains_resource::<TransformGuard>() {
        return;
    }
    let now = world.change_tick();
    let moved = |changed: ChangeTick, added: ChangeTick| {
        changed.is_newer_than(started, now) && !added.is_newer_than(started, now)
    };
    let mut offenders = Vec::new();
    let mut locals = world.query::<(Entity, Ref<Transform>, Option<&Name>)>();
    for (entity, transform, name) in locals.iter(world) {
        if moved(transform.last_changed(), transform.added()) {
            offenders.push(format!("{entity} ({}) Transform", describe(name)));
        }
    }
    let mut globals = world.query::<(Entity, Ref<GlobalTransform>, Option<&Name>)>();
    for (entity, transform, name) in globals.iter(world) {
        if moved(transform.last_changed(), transform.added()) {
            offenders.push(format!("{entity} ({}) GlobalTransform", describe(name)));
        }
    }
    if !offenders.is_empty() {
        panic!(
            "TransformGuard: the simulation wrote a transform it did not create: {}. What is \
             drawn is built in `TickedPresentation`, from simulated state, after the tick; a \
             transform written inside it is rewritten by every replay and left behind by \
             anything that moves the body afterwards.",
            offenders.join(", ")
        );
    }
}

fn describe(name: Option<&Name>) -> String {
    name.map_or_else(|| "unnamed".to_string(), |name| name.as_str().to_string())
}

/// Panic if a system in the simulation declares access to `Transform` or `GlobalTransform`.
///
/// At startup, while the schedule is still a graph: once it has run, the systems have been moved
/// into the executor and their access is no longer public. Initialising a system here is the
/// same call the executor makes on its first run, which reuses the state this one creates.
fn audit_simulation_reads(world: &mut World) {
    let guard = world.resource::<TransformGuard>();
    if guard.audited {
        return;
    }
    let allowed = guard.allowed.clone();
    world.resource_mut::<TransformGuard>().audited = true;

    let transform = world.register_component::<Transform>();
    let global = world.register_component::<GlobalTransform>();
    let Some(mut schedule) = world.resource_mut::<Schedules>().remove(TickedSimulation) else {
        return;
    };

    let mut offenders = Vec::new();
    let mut unnamed = false;
    let graph = schedule.graph_mut();
    let keys: Vec<_> = graph.systems.iter().map(|(key, ..)| key).collect();
    for key in keys {
        let Some(system) = graph.systems.get_mut(key) else {
            continue;
        };
        let access = system.initialize(world);
        let access = access.combined_access();
        if access.has_read_all() {
            continue;
        }
        if !(access.has_read(transform) || access.has_read(global)) {
            continue;
        }
        let name = system.system().name().to_string();
        // Without bevy's `debug` feature every name is the same placeholder, and an allow-list
        // keyed on names cannot tell a dependency from the game.
        if !name.contains("::") {
            unnamed = true;
            continue;
        }
        if allowed
            .iter()
            .any(|prefix| name.starts_with(prefix.as_str()))
        {
            continue;
        }
        offenders.push(name);
    }
    world.resource_mut::<Schedules>().insert(schedule);

    if unnamed {
        warn!(
            "TransformGuard: system names are not available (bevy's `debug` feature is off), so \
             the startup check for transform reads in the simulation was skipped"
        );
    }
    if !offenders.is_empty() {
        panic!(
            "TransformGuard: these systems in `TickedSimulation` read or write `Transform` or \
             `GlobalTransform`: {}. The simulation works on its own components; the transform \
             is built from them in `TickedPresentation`.",
            offenders.join(", ")
        );
    }
}
