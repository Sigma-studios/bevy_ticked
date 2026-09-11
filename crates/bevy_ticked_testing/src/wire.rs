//! Pin a registry's shape, so a wire-format change fails on the commit that makes it.
//!
//! A snapshot names a component or resource by a `u16` wire index, and nothing in a packet says
//! which type an index means. Two peers built from different commits deserialise each other's
//! `Position` bytes as an `EntityKind` — with no error, no warning, and no way to tell from the
//! symptom that the cause is a build mismatch rather than a physics bug. The handshake in
//! `bevy_ticked_networking_ensemble` catches that at the join, by exchanging the sorted names.
//! These assertions catch it in CI, on the commit.
//!
//! Write the expected list by hand next to the assertion rather than reading it out of the
//! registry: a list that derived itself from the thing it is checking could not disagree with
//! it.
//!
//! # Name-derived: order of registration no longer matters
//!
//! Since the snapshot wire v2 phase the ticked registries derive a type's wire index from its
//! rank among the **sorted** wire names, exactly as `bevy_ensemble`'s message registry does.
//! Tidying the registration block into alphabetical order, or splitting it across plugins that
//! build in a different order, is no longer a wire change. Adding, removing or renaming a
//! networked type still is, and so is a rollback-only type gaining a wire name; those are what
//! [`assert_wire_order`], [`assert_resource_wire_order`] and [`assert_wire_names`] catch.
//! Every list they take is compared as a set, so the call site may list names in whichever
//! order reads best.
//!
//! Reading the wire names freezes the registry; call these after every plugin has been built.

use bevy::prelude::*;
use bevy_ensemble::{EnsembleMessage, EnsembleMessageRegistry};
use bevy_ticked::registry::TickedComponentRegistry;
use bevy_ticked::resource_registry::TickedResourceRegistry;

/// The networked component wire names equal `expected`, as a set.
///
/// The name is historical: the order in `expected` does not matter any more, and neither does
/// the order of registration. Adding, removing or renaming a networked component changes the
/// sorted list, and with it every wire index after the change and the registry hash the join
/// handshake compares.
#[track_caller]
pub fn assert_wire_order(registry: &TickedComponentRegistry, expected: &[&str]) {
    let names: Vec<&str> = registry.wire_names().collect();
    let mut expected: Vec<&str> = expected.to_vec();
    expected.sort_unstable();
    assert_eq!(
        names, expected,
        "the set of networked components changed, and the sorted names are the wire format. \
         Adding, removing or renaming a networked component moves every index after it and \
         changes the registry hash the join handshake compares, so every older build refuses \
         the join. Reordering registrations is not a change. If the change is intended, update \
         the expected list and say so in the commit."
    );
}

/// The networked resource wire names equal `expected`, as a set.
///
/// Same rule as [`assert_wire_order`], for the resource registry.
#[track_caller]
pub fn assert_resource_wire_order(registry: &TickedResourceRegistry, expected: &[&str]) {
    let names: Vec<&str> = registry.wire_names().collect();
    let mut expected: Vec<&str> = expected.to_vec();
    expected.sort_unstable();
    assert_eq!(
        names, expected,
        "the set of networked resources changed, and the sorted names are the wire format. \
         Adding, removing or renaming a networked resource moves every index after it and \
         changes the registry hash the join handshake compares, so every older build refuses \
         the join. Reordering registrations is not a change. If the change is intended, update \
         the expected list and say so in the commit."
    );
}

/// This app's networked component wire names equal `expected`, as a set.
///
/// [`assert_wire_order`] over the app's `TickedComponentRegistry`, for the common case of a
/// game pinning the shape of a fully built app.
///
/// # Panics
///
/// If the app has no `TickedComponentRegistry` (no `TickedPlugin`).
#[track_caller]
pub fn assert_wire_names(app: &App, expected: &[&str]) {
    assert_wire_order(component_registry(app), expected);
}

/// This app's networked resource wire names equal `expected`, as a set. An app with no
/// resource registry has no networked resources, and `expected` must be empty.
#[track_caller]
pub fn assert_resource_wire_names(app: &App, expected: &[&str]) {
    match app.world().get_resource::<TickedResourceRegistry>() {
        Some(registry) => assert_resource_wire_order(registry, expected),
        None => assert!(
            expected.is_empty(),
            "the app has no TickedResourceRegistry, so no resource is on the wire; expected \
             {expected:?}"
        ),
    }
}

/// The component registry's wire hash: `PROTOCOL_VERSION` folded with the sorted networked
/// names. The number the join handshake compares, so two apps with equal hashes read each
/// other's snapshots and two with different ones refuse the join. Pin it in a test to be told
/// on the commit that changes the wire.
///
/// Reading it freezes the registry.
#[track_caller]
pub fn wire_hash(app: &App) -> u64 {
    component_registry(app).wire_hash()
}

/// The resource registry's wire hash, `0` for an app without one — which is what the
/// handshake sends for it.
#[track_caller]
pub fn resource_wire_hash(app: &App) -> u64 {
    app.world()
        .get_resource::<TickedResourceRegistry>()
        .map_or(0, TickedResourceRegistry::wire_hash)
}

#[track_caller]
fn component_registry(app: &App) -> &TickedComponentRegistry {
    app.world()
        .get_resource::<TickedComponentRegistry>()
        .expect("the app has no TickedComponentRegistry; add TickedPlugin first")
}

/// The wire index `T` travels under in this app's `bevy_ensemble` registry.
///
/// Reading an index freezes the registry, so call this after every plugin has been built;
/// registering a message afterwards panics in `bevy_ensemble`.
///
/// # Panics
///
/// If the app has no [`EnsembleMessageRegistry`], or `T` was never registered.
#[track_caller]
pub fn ensemble_index_of<T: EnsembleMessage>(app: &App) -> u16 {
    ensemble_registry(app).index_of::<T>().unwrap_or_else(|| {
        panic!(
            "`{}` is not registered as an ensemble message in this app",
            std::any::type_name::<T>()
        )
    })
}

/// Every ensemble message wire name, in wire-index order.
///
/// `bevy_ensemble` derives indices from sorted names, so this is the sorted name set, and it
/// **is** that registry's wire format. Reading it freezes the registry; see
/// [`ensemble_index_of`].
#[track_caller]
pub fn ensemble_wire_names(app: &App) -> Vec<&'static str> {
    ensemble_registry(app).wire_names()
}

/// The ensemble wire names equal `expected`.
///
/// Because the indices are name-derived, only adding, removing or renaming a message changes
/// this list. Any of those changes the protocol hash and makes every older build refuse the
/// join, which is what this pins in CI.
#[track_caller]
pub fn assert_ensemble_wire_names(app: &App, expected: &[&str]) {
    let names = ensemble_wire_names(app);
    assert_eq!(
        names, expected,
        "the set of ensemble messages changed, and the sorted names are the wire format. \
         Adding, removing or renaming a message changes the protocol hash and every older build \
         refuses the join. If the change is intended, update the expected list and say so in the \
         commit."
    );
}

#[track_caller]
fn ensemble_registry(app: &App) -> &EnsembleMessageRegistry {
    app.world()
        .get_resource::<EnsembleMessageRegistry>()
        .expect("the app has no EnsembleMessageRegistry; add an ensemble plugin first")
}
