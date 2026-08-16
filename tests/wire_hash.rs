//! What `wire_hash()` does and does not notice.
//!
//! The hash existed for a long time with a doc comment telling people to exchange it at join time,
//! and nobody did — so these are the first tests it has ever had, and two of them are about the
//! reason it was not safe to wire up: an entry registered without an explicit name was hashed
//! under `std::any::type_name`, whose output is explicitly not specified across compiler versions.

use bevy::prelude::*;
use bevy_ticked::prelude::*;
use bevy_ticked::registry::TickedComponentRegistry;
use bevy_ticked::resource_registry::TickedResourceRegistry;

#[derive(Component, Clone, Copy)]
struct Pos(i32);

#[derive(Component, Clone, Copy)]
struct Vel(i32);

#[derive(Component, Clone, Copy)]
struct Tag;

#[derive(Resource, Clone, Copy)]
struct Round(u32);

#[derive(Resource, Clone, Copy)]
struct Score(u32);

fn registry(build: impl FnOnce(&mut App)) -> TickedComponentRegistry {
    let mut app = App::new();
    app.init_resource::<TickedComponentRegistry>();
    build(&mut app);
    app.world().resource::<TickedComponentRegistry>().clone()
}

fn resources(build: impl FnOnce(&mut App)) -> TickedResourceRegistry {
    let mut app = App::new();
    app.init_resource::<TickedResourceRegistry>();
    build(&mut app);
    app.world().resource::<TickedResourceRegistry>().clone()
}

// ── what it must catch ───────────────────────────────────────────────────────

/// The failure the whole thing exists for: same types, different order, so every index from the
/// first difference onward means something else.
#[test]
fn a_reordered_registration_changes_the_hash() {
    let one = registry(|app| {
        app.register_ticked_component_as::<Pos>("Pos")
            .register_ticked_component_as::<Vel>("Vel");
    });
    let other = registry(|app| {
        app.register_ticked_component_as::<Vel>("Vel")
            .register_ticked_component_as::<Pos>("Pos");
    });

    assert_ne!(
        one.wire_hash(),
        other.wire_hash(),
        "the multiset of names is the same and the meaning of index 0 is not"
    );
}

#[test]
fn an_extra_registration_changes_the_hash() {
    let one = registry(|app| {
        app.register_ticked_component_as::<Pos>("Pos");
    });
    let other = registry(|app| {
        app.register_ticked_component_as::<Pos>("Pos")
            .register_ticked_component_as::<Vel>("Vel");
    });
    assert_ne!(one.wire_hash(), other.wire_hash());
}

/// The case that made skipping unnamed entries unsafe. An unnamed type shifts every index after
/// it, so if it contributed nothing the two registries would hash equal while disagreeing about
/// what index 1 means.
#[test]
fn an_extra_unnamed_registration_still_changes_the_hash() {
    let one = registry(|app| {
        app.register_ticked_component_as::<Pos>("Pos")
            .register_ticked_component_as::<Vel>("Vel");
    });
    let other = registry(|app| {
        app.register_ticked_component_as::<Pos>("Pos")
            .register_ticked_component::<Tag>()
            .register_ticked_component_as::<Vel>("Vel");
    });

    assert_ne!(
        one.wire_hash(),
        other.wire_hash(),
        "an unnamed type contributes no name but it does occupy an index, and the index is what \
         a snapshot is keyed by"
    );
}

/// Resources are their own index space, so the component hash cannot speak for them.
#[test]
fn the_resource_hash_is_a_separate_number() {
    let one = resources(|app| {
        app.register_ticked_resource::<Round>();
    });
    let other = resources(|app| {
        app.register_ticked_resource::<Round>();
        app.register_ticked_resource::<Score>();
    });

    assert_ne!(
        one.wire_hash(),
        other.wire_hash(),
        "a peer can agree about every component and still disagree here"
    );
}

// ── what it must not catch ───────────────────────────────────────────────────

/// The reason this was not safe to exchange before.
///
/// `register_ticked_component` defaults the wire name to `std::any::type_name`, which is
/// explicitly not specified across compiler versions. Two peers built from the same source on
/// different rustc releases would have reported disagreeing registries — a loud error for a
/// non-problem, which is how a check gets switched off within a week.
///
/// Simulated by registering two *different* unnamed types in the same position: if the name were
/// folded in, these would hash differently, which is precisely what a rustc difference would do to
/// one type.
#[test]
fn an_unnamed_types_name_does_not_reach_the_hash() {
    let one = registry(|app| {
        app.register_ticked_component_as::<Pos>("Pos")
            .register_ticked_component::<Vel>();
    });
    let other = registry(|app| {
        app.register_ticked_component_as::<Pos>("Pos")
            .register_ticked_component::<Tag>();
    });

    assert_eq!(
        one.wire_hash(),
        other.wire_hash(),
        "an unnamed entry contributes its position and a sentinel, never a string whose spelling \
         is up to the compiler"
    );
}

/// Naming a rollback-only type opts it back in, which is what `register_ticked_component_as` is
/// for: the handshake can then say *which* registration differs rather than only that the shapes
/// do.
#[test]
fn naming_a_rollback_only_type_puts_it_back_in_the_hash() {
    let unnamed = registry(|app| {
        app.register_ticked_component_as::<Pos>("Pos")
            .register_ticked_component::<Vel>();
    });
    let named = registry(|app| {
        app.register_ticked_component_as::<Pos>("Pos")
            .register_ticked_component_as::<Vel>("Vel");
    });

    assert_ne!(unnamed.wire_hash(), named.wire_hash());
}

/// Two identically-built peers, which is every session this stack has ever run.
#[test]
fn the_same_registrations_hash_the_same() {
    let build = |app: &mut App| {
        app.register_ticked_component_as::<Pos>("Pos")
            .register_ticked_component_as::<Vel>("Vel")
            .register_ticked_component::<Tag>();
    };
    assert_eq!(registry(build).wire_hash(), registry(build).wire_hash());
}

/// Renaming the Rust type is free as long as the wire name is unchanged — the whole point of
/// spelling the names out.
#[test]
fn the_hash_follows_the_wire_name_and_not_the_rust_type() {
    let one = registry(|app| {
        app.register_ticked_component_as::<Pos>("Position");
    });
    let other = registry(|app| {
        // A different Rust type entirely, under the same wire name.
        app.register_ticked_component_as::<Vel>("Position");
    });
    assert_eq!(one.wire_hash(), other.wire_hash());
}
