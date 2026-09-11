//! What the wire format is, and what `wire_hash()` does and does not notice.
//!
//! The format is the sorted list of networked names and nothing else. Registration order used
//! to be the format: indices were assigned by position, and two peers that registered in a
//! different order read each other's `Position` bytes as something else with no error of any
//! kind. Now the index is derived from the name, so the order cannot matter, a rollback-only
//! type cannot shift anything, and the handshake can say which name differs.

use bevy::prelude::*;
use bevy_ticked::prelude::*;
use bevy_ticked::registry::{PROTOCOL_VERSION, TickedComponentRegistry, WireFns};
use bevy_ticked::resource_registry::TickedResourceRegistry;

#[derive(Component, Clone, Copy)]
struct Pos(#[allow(dead_code)] i32);

#[derive(Component, Clone, Copy)]
struct Vel(#[allow(dead_code)] i32);

#[derive(Component, Clone, Copy)]
struct Tag;

#[derive(Resource, Clone, Copy, Default)]
struct Round(#[allow(dead_code)] u32);

#[derive(Resource, Clone, Copy, Default)]
struct Score(#[allow(dead_code)] u32);

/// The networking crate supplies real ones; the format does not depend on them.
fn stub_wire() -> WireFns {
    WireFns {
        encode_one: |_, _, _, _| false,
        decode_one: |_, _, _, _, _| None,
        begin_tick: |_, _| {},
        finish_tick: |_, _| {},
        has_at: |_, _, _| false,
    }
}

fn networked<T: TickedComponent>(app: &mut App, name: &'static str) {
    app.init_resource::<TickedComponentRegistry>();
    app.init_resource::<WorldActions<T>>();
    app.world_mut()
        .resource_mut::<TickedComponentRegistry>()
        .register_networked::<T>(name, stub_wire());
}

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

fn stub_serialize(_: &World, _: u64) -> Option<Vec<u8>> {
    None
}
fn stub_apply(_: &mut World, _: u64, _: &[u8]) {}

// ── the format ───────────────────────────────────────────────────────────────

/// The failure the old format had: same types, different order, different meaning of index 0.
#[test]
fn registration_order_does_not_change_the_wire_format() {
    let one = registry(|app| {
        networked::<Pos>(app, "Pos");
        networked::<Vel>(app, "Vel");
    });
    let other = registry(|app| {
        networked::<Vel>(app, "Vel");
        networked::<Pos>(app, "Pos");
    });

    assert_eq!(one.wire_hash(), other.wire_hash());
    assert_eq!(
        one.wire_names().collect::<Vec<_>>(),
        other.wire_names().collect::<Vec<_>>()
    );
}

#[test]
fn derived_indices_are_stable_under_reordering() {
    let one = registry(|app| {
        networked::<Pos>(app, "Pos");
        networked::<Vel>(app, "Vel");
    });
    let other = registry(|app| {
        networked::<Vel>(app, "Vel");
        networked::<Pos>(app, "Pos");
    });
    assert_eq!(one.wire_index_of::<Pos>(), other.wire_index_of::<Pos>());
    assert_eq!(one.wire_index_of::<Vel>(), other.wire_index_of::<Vel>());
    assert_eq!(one.wire_index_of::<Pos>(), Some(0), "\"Pos\" sorts before \"Vel\"");
    assert_eq!(one.wire_index_of::<Vel>(), Some(1));
    // The registration index is a different number and says so in its name.
    assert_ne!(one.index_of::<Pos>(), other.index_of::<Pos>());
}

#[test]
fn an_extra_networked_registration_changes_the_hash() {
    let one = registry(|app| networked::<Pos>(app, "Pos"));
    let other = registry(|app| {
        networked::<Pos>(app, "Pos");
        networked::<Vel>(app, "Vel");
    });
    assert_ne!(one.wire_hash(), other.wire_hash());
}

/// A rollback-only type never travels, so a peer with an extra one agrees about every byte.
#[test]
fn a_rollback_only_type_is_not_on_the_wire() {
    let one = registry(|app| {
        networked::<Pos>(app, "Pos");
        networked::<Vel>(app, "Vel");
    });
    let other = registry(|app| {
        networked::<Pos>(app, "Pos");
        app.register_ticked_component::<Tag>();
        networked::<Vel>(app, "Vel");
    });
    assert_eq!(one.wire_hash(), other.wire_hash());
    assert_eq!(other.wire_index_of::<Tag>(), None);
    assert_eq!(other.wire_len(), 2);
    assert_eq!(other.len(), 3, "it is still registered, and still rolled back");
}

#[test]
fn two_types_with_the_same_wire_name_are_refused() {
    let outcome = std::panic::catch_unwind(|| {
        registry(|app| {
            networked::<Pos>(app, "Position");
            networked::<Vel>(app, "Position");
        })
    });
    let Err(payload) = outcome else {
        panic!("a duplicate name must panic");
    };
    let message = payload.downcast_ref::<String>().cloned().unwrap_or_default();
    assert!(
        message.contains("Position") && message.contains("share the wire name"),
        "the panic names the duplicate: {message}"
    );
}

#[test]
fn registering_after_the_format_is_frozen_panics() {
    let outcome = std::panic::catch_unwind(|| {
        registry(|app| {
            networked::<Pos>(app, "Pos");
            // Anything that reads the wire order freezes it.
            let _ = app
                .world()
                .resource::<TickedComponentRegistry>()
                .wire_hash();
            networked::<Vel>(app, "Vel");
        })
    });
    let Err(payload) = outcome else {
        panic!("registering after the freeze must panic");
    };
    let message = payload.downcast_ref::<String>().cloned().unwrap_or_default();
    assert!(message.contains("frozen"), "{message}");
}

#[test]
fn the_registry_is_not_frozen_until_something_asks() {
    let one = registry(|app| {
        networked::<Pos>(app, "Pos");
        assert!(
            !app.world().resource::<TickedComponentRegistry>().is_frozen(),
            "registration alone does not freeze"
        );
    });
    assert!(!one.is_frozen());
    let _ = one.wire_len();
    assert!(one.is_frozen());
}

/// The protocol version is in the hash, so a client and host with the same registrations
/// but a different encoding refuse each other.
#[test]
fn the_hash_folds_the_protocol_version() {
    assert_eq!(PROTOCOL_VERSION, 2);
    let one = registry(|app| networked::<Pos>(app, "Pos"));
    // FNV over version then names; a plain FNV over the names would differ.
    let names_only = bevy_ticked_testing_free_fnv(&["Pos"]);
    assert_ne!(one.wire_hash(), names_only);
}

fn bevy_ticked_testing_free_fnv(names: &[&str]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for name in names {
        for byte in name.as_bytes().iter().chain(b"\0") {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}

/// Resources are their own index space, so the component hash cannot speak for them.
#[test]
fn the_resource_hash_is_a_separate_number() {
    let one = resources(|app| {
        app.init_resource::<TickedResourceRegistry>();
        app.world_mut()
            .resource_mut::<TickedResourceRegistry>()
            .register_networked::<Round>("Round", stub_serialize, stub_apply);
    });
    let other = resources(|app| {
        let mut registry = app.world_mut().resource_mut::<TickedResourceRegistry>();
        registry.register_networked::<Round>("Round", stub_serialize, stub_apply);
        registry.register_networked::<Score>("Score", stub_serialize, stub_apply);
    });

    assert_ne!(
        one.wire_hash(),
        other.wire_hash(),
        "a peer can agree about every component and still disagree here"
    );
    assert_eq!(other.wire_index_of::<Round>(), Some(0));
    assert_eq!(other.wire_index_of::<Score>(), Some(1));
}

/// Two identically-built peers, which is every session this stack has ever run.
#[test]
fn the_same_registrations_hash_the_same() {
    let build = |app: &mut App| {
        networked::<Pos>(app, "Pos");
        networked::<Vel>(app, "Vel");
        app.register_ticked_component::<Tag>();
    };
    assert_eq!(registry(build).wire_hash(), registry(build).wire_hash());
}

/// Renaming the Rust type is free as long as the wire name is unchanged — the whole point of
/// spelling the names out.
#[test]
fn the_hash_follows_the_wire_name_and_not_the_rust_type() {
    let one = registry(|app| networked::<Pos>(app, "Position"));
    let other = registry(|app| networked::<Vel>(app, "Position"));
    assert_eq!(one.wire_hash(), other.wire_hash());
}
