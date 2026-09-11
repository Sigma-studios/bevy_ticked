//! Pin a registry's shape, so a wire-format change fails on the commit that makes it.
//!
//! Snapshot component and resource indices are positional `u16`s: the `n`th registered type
//! travels as `n`. Nothing in a packet says which type an index means, so two peers built from
//! different commits deserialise each other's `Position` bytes as an `EntityKind` — with no
//! error, no warning, and no way to tell from the symptom that the cause is a build mismatch
//! rather than a physics bug. The handshake in `bevy_ticked_networking_ensemble` catches that at
//! the join, by exchanging `wire_hash()`. These assertions catch it in CI, on the commit.
//!
//! Write the expected list by hand next to the assertion rather than reading it out of the
//! registry: a list that derived itself from the thing it is checking could not disagree with
//! it. This is what fails when somebody tidies the registration block into alphabetical order —
//! which compiles, warns about nothing, and changes the meaning of every index.
//!
//! # Positional today, name-derived later
//!
//! The ticked registries' positional scheme is being replaced by name-derived indices (sorted
//! wire names, as `bevy_ensemble`'s message registry already does) in a later phase of the
//! overhaul. When that lands, [`assert_wire_order`] and [`assert_resource_wire_order`] will
//! compare the *sorted name set* rather than the registration order, and a reorder will stop
//! being a wire change. The call sites will not need to change; only what counts as a mismatch
//! will. The ensemble assertions below already work that way, because that registry already
//! does.

use bevy::prelude::*;
use bevy_ensemble::{EnsembleMessage, EnsembleMessageRegistry};
use bevy_ticked::registry::TickedComponentRegistry;
use bevy_ticked::resource_registry::TickedResourceRegistry;

/// The registered component wire names, in wire-index order, equal `expected`.
///
/// Appending to the registration block is safe; reordering, renaming and deleting are not,
/// because the index is the position and every snapshot carries indices.
#[track_caller]
pub fn assert_wire_order(registry: &TickedComponentRegistry, expected: &[&str]) {
    let names: Vec<&str> = registry.wire_names().collect();
    assert_eq!(
        names, expected,
        "the component registration order changed, and the order is the wire format. \
         Appending is safe; reordering, renaming and deleting are not: indices are positional \
         and travel in every snapshot. If the change is intended, update the expected list and \
         say so in the commit."
    );
}

/// The registered resource wire names, in wire-index order, equal `expected`.
///
/// Same rule as [`assert_wire_order`], for the resource registry.
#[track_caller]
pub fn assert_resource_wire_order(registry: &TickedResourceRegistry, expected: &[&str]) {
    let names: Vec<&str> = registry.wire_names().collect();
    assert_eq!(
        names, expected,
        "the resource registration order changed, and the order is the wire format. \
         Appending is safe; reordering, renaming and deleting are not: indices are positional \
         and travel in every snapshot. If the change is intended, update the expected list and \
         say so in the commit."
    );
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
