//! Simulations to run the harness against.
//!
//! [`minimal`] is the smallest game that exercises every path the harness asserts about: tracked
//! entities with networked state, an input that moves them, a local-only marker attached by an
//! observer, and a world hash. Integer positions, so two peers that agree are bit-identical and a
//! divergence is a fact rather than a tolerance.
//!
//! It exists so the harness can prove it bites (`tests/bites.rs`) without depending on any real
//! game, and so a game's own test can start from something known to work.

pub mod minimal {
    //! One integer axis, one input, one body per player.

    use bevy::prelude::*;
    use bevy_ticked::TickedSimulation;
    use bevy_ticked::checksum::WorldHash;
    use bevy_ticked::interpolation::{TickedInterpolation, TickedInterpolationPlugin};
    use bevy_ticked::lifetimes::TickedEntityCommandsExt;
    use bevy_ticked::registry::TickedAppExt;
    use bevy_ticked::tick::CurrentTick;
    use bevy_ticked::tracked_entity::{
        LocalSpawnerSlot, SpawnerSlot, TickTrackedEntity, TrackedIdAllocator, TrackedSpawner,
    };
    use bevy_ticked_networking::input::InputQueue;
    use bevy_ticked_networking::networked_registry::NetworkedTickedAppExt;
    use serde::{Deserialize, Serialize};

    use crate::net::TickedNetwork;
    use crate::peer::TICK;

    /// Position along the one axis. Networked.
    #[derive(Component, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Debug)]
    pub struct Pos(pub i64);

    /// Velocity along the one axis, in units per tick. Networked; set from input.
    #[derive(Component, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Debug)]
    pub struct Vel(pub i64);

    /// What kind of thing this is. Networked, so a client can tell a player from a pellet.
    #[derive(Component, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Debug)]
    pub struct EntityKind(pub u8);

    impl EntityKind {
        pub const PLAYER: Self = Self(1);
        pub const PELLET: Self = Self(2);
    }

    /// Which player drives this body: the stack's own, so a client predicts its body and
    /// interpolates everybody else's.
    pub use bevy_ticked_networking::replication::Owner;

    /// What a player presses: a direction, or nothing, and whether they fire.
    #[derive(Clone, Copy, Default, Serialize, Deserialize, Debug, PartialEq, Eq)]
    pub struct Input {
        pub dx: i8,
        pub fire: bool,
    }

    impl Input {
        pub const RIGHT: Self = Self { dx: 1, fire: false };
        pub const LEFT: Self = Self {
            dx: -1,
            fire: false,
        };
        pub const NONE: Self = Self { dx: 0, fire: false };
        pub const FIRE: Self = Self { dx: 0, fire: true };
    }

    /// The slot a player's peer mints under, on their body, so every peer spawns that
    /// player's pellets under the same ids. Networked; the host sets it when it seats a
    /// player (0 for itself, the welcomed slot for a client).
    #[derive(Component, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Debug)]
    pub struct PlayerSlot(pub u8);

    /// A pellet's remaining life in ticks; it is `despawn_ticked` at zero.
    #[derive(Component, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Debug)]
    pub struct Fuse(pub u8);

    /// How long a pellet lives.
    pub const PELLET_LIFE: u8 = 96;

    /// Local-only view state, attached to every tracked entity by an observer.
    ///
    /// It is here to be *not* networked: a snapshot must not strip it, a rollback must not touch
    /// it, and the wire-shape pin must not list it.
    #[derive(Component, Debug, Default)]
    pub struct Visual;

    /// The tick phases, so the checksum can be told to sample a finished tick rather than a
    /// half-simulated one. Unordered against an exclusive sampling system, the executor would
    /// pick an order per peer, and two peers hashing the same world at different points in the
    /// tick disagree about every tick.
    #[derive(SystemSet, Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub enum MinimalSet {
        Simulate,
        Sample,
    }

    fn attach_visual(add: On<Add, TickTrackedEntity>, mut commands: Commands) {
        commands.entity(add.entity).insert(Visual);
    }

    /// Set every body's velocity from its owner's input for this tick, or from the newest one
    /// before it: a player with no input filed for this tick keeps pressing what they last
    /// pressed. A player with nothing at all leaves the last velocity standing.
    ///
    /// Hold-last rather than `at_tick`, because a remote player's input for a tick this peer
    /// has not received yet is far more likely "still walking" than "stopped": with `at_tick`
    /// a predicted remote body stood still for every tick past the relayed inputs and snapped
    /// forward when the snapshot arrived.
    pub fn apply_inputs(
        tick: Res<CurrentTick>,
        queue: Res<InputQueue<Input>>,
        mut bodies: Query<(&Owner, &mut Vel)>,
    ) {
        let inputs = queue.at_tick_or_last(tick.0);
        for (owner, mut vel) in &mut bodies {
            if let Some(input) = inputs.get(&owner.0) {
                vel.0 = i64::from(input.dx);
            }
        }
    }

    /// A pellet for every player who fired this tick, minted under that player's slot so the
    /// host and every client mint the same id: the spawn a client predicts and the host
    /// confirms. Runs on every peer, from the same relayed inputs.
    pub fn fire_pellets(
        tick: Res<CurrentTick>,
        queue: Res<InputQueue<Input>>,
        players: Query<(&Owner, &Pos, &PlayerSlot)>,
        mut spawner: TrackedSpawner,
    ) {
        let Some(inputs) = queue.at_tick(tick.0) else {
            return;
        };
        let mut shooters: Vec<(u8, i64)> = players
            .iter()
            .filter(|(owner, _, _)| inputs.get(&owner.0).is_some_and(|input| input.fire))
            .map(|(_, pos, slot)| (slot.0, pos.0))
            .collect();
        shooters.sort_unstable();
        for (slot, pos) in shooters {
            spawner.spawn_by(
                SpawnerSlot(slot),
                (Pos(pos), Vel(1), EntityKind::PELLET, Fuse(PELLET_LIFE)),
            );
        }
    }

    /// Pellets burn down and are tombstoned, never destroyed.
    pub fn burn_fuses(mut pellets: Query<(Entity, &mut Fuse)>, mut commands: Commands) {
        for (entity, mut fuse) in &mut pellets {
            if fuse.0 == 0 {
                commands.entity(entity).despawn_ticked();
            } else {
                fuse.0 -= 1;
            }
        }
    }

    /// `Pos += Vel`, once per tick.
    ///
    /// Reads `Time` only to check that the tick clock is installed: inside a tick `delta` is
    /// exactly one tick, whatever frame or rollback is running it, and a fixture that saw
    /// anything else would be running outside `run_tick_schedule`.
    pub fn integrate(time: Res<Time>, mut bodies: Query<(&mut Pos, &Vel)>) {
        debug_assert_eq!(
            time.delta(),
            TICK,
            "a tick was run without the tick clock installed"
        );
        for (mut pos, vel) in &mut bodies {
            pos.0 += vel.0;
        }
    }

    /// Mint a body for `uuid` under the authority's slot. On a host, or on a solo peer; a
    /// client doing this is the bug `assert_no_id_unissued` exists to catch.
    pub fn spawn_player(world: &mut World, uuid: u128) -> Entity {
        let id = world.resource_mut::<TrackedIdAllocator>().next_authority();
        world
            .spawn((Pos(0), Vel(0), EntityKind::PLAYER, Owner(uuid), id))
            .id()
    }

    /// Register the four networked components under stable wire names.
    ///
    /// Separate from [`install_systems`] so a test can build a peer that registers a different
    /// set — the way a peer built from another commit would — and watch the harness notice.
    pub fn register_components(app: &mut App) {
        app.register_networked_ticked_component::<Pos>("Pos")
            .register_networked_ticked_component::<Vel>("Vel")
            .register_networked_ticked_component_once::<EntityKind>("EntityKind")
            .register_networked_ticked_component::<PlayerSlot>("PlayerSlot")
            .register_networked_ticked_component::<Fuse>("Fuse");
        // `Owner` is the stack's own and is registered by the role plugins.
    }

    /// The simulation systems and the `Visual` observer, without any registration.
    pub fn install_systems(app: &mut App) {
        app.configure_sets(
            TickedSimulation,
            (MinimalSet::Simulate, MinimalSet::Sample).chain(),
        )
        .add_systems(
            TickedSimulation,
            (apply_inputs, integrate, fire_pellets, burn_fuses)
                .chain()
                .in_set(MinimalSet::Simulate),
        )
        .add_observer(attach_visual);
    }

    /// The whole fixture: registrations and systems.
    pub fn install(app: &mut App) {
        register_components(app);
        install_systems(app);
    }

    /// [`install`] plus a per-tick [`MinimalHash`] log, sampled after the simulation has run.
    pub fn install_with_checksums(app: &mut App) {
        install(app);
        app.insert_resource(bevy_ticked::checksum::ChecksumLog::<MinimalHash>::every_tick());
        app.add_plugins(
            bevy_ticked::checksum::ChecksumLogPlugin::<MinimalHash>::default()
                .in_set(MinimalSet::Sample),
        );
    }

    // ── With a transform ─────────────────────────────────────────────────────────────────────

    /// [`install`] plus a `Transform` on every tracked entity, written from `Pos` each tick,
    /// and a `TickedInterpolation` to blend it between ticks.
    ///
    /// Opt-in: the integer fixture is exact and a float transform is not, and neither the tick
    /// interpolation nor correction smoothing (`TickedSmoothingPlugin`) has anything to do
    /// without a `Transform`. A test about how a body is *drawn* installs this; one about
    /// where it *is* does not.
    ///
    /// `Transform` is registered for rollback (never the wire), so a replay restores it and
    /// re-derives it from the replayed `Pos` rather than carrying the blend the renderer last
    /// wrote — the T4 failure — and so a corrupted transform is put right by the next snapshot
    /// the way a corrupted `Pos` is.
    pub fn install_with_transform(app: &mut App) {
        install(app);
        app.register_ticked_component_as::<Transform>("Transform")
            .add_plugins(TickedInterpolationPlugin)
            .add_observer(attach_transform)
            .add_systems(
                TickedSimulation,
                sync_transform.after(integrate).in_set(MinimalSet::Simulate),
            );
    }

    fn transform_at(pos: i64) -> Transform {
        Transform::from_xyz(pos as f32, 0.0, 0.0)
    }

    /// Every tracked entity gets a transform and an interpolation state, whichever peer spawned
    /// it: a body a snapshot spawns on a client arrives with its networked components only.
    fn attach_transform(
        add: On<Add, TickTrackedEntity>,
        bodies: Query<(Option<&Pos>, Has<Transform>)>,
        mut commands: Commands,
    ) {
        let Ok((pos, has_transform)) = bodies.get(add.entity) else {
            return;
        };
        let mut entity = commands.entity(add.entity);
        entity.insert_if_new(TickedInterpolation::default());
        if !has_transform {
            entity.insert(transform_at(pos.map_or(0, |pos| pos.0)));
        }
    }

    /// `Transform.translation.x = Pos`, after the integration, so the blend and the smoothing
    /// offset have a float to work on.
    ///
    /// Self-healing: a rollback restore removes a rollback-only component from every entity
    /// the restored tick has no record of, and a body a snapshot spawned mid-replay is exactly
    /// that for the ticks before its first capture. A body found bare gets its transform back
    /// before this tick is captured, so it is never bare twice.
    pub fn sync_transform(
        mut bodies: Query<(Entity, &Pos, Option<&mut Transform>), With<TickTrackedEntity>>,
        mut commands: Commands,
    ) {
        for (entity, pos, transform) in &mut bodies {
            match transform {
                Some(mut transform) => transform.translation.x = pos.0 as f32,
                None => {
                    commands.entity(entity).insert(transform_at(pos.0));
                }
            }
        }
    }

    /// [`spawn_player`] with the transform and interpolation state already on the body, so its
    /// tick-0 capture holds a real transform rather than the observer's deferred one. Only
    /// meaningful under [`install_with_transform`]; elsewhere the transform is dead weight.
    pub fn spawn_player_with_transform(world: &mut World, uuid: u128) -> Entity {
        let id = world.resource_mut::<TrackedIdAllocator>().next_authority();
        world
            .spawn((
                Pos(0),
                Vel(0),
                EntityKind::PLAYER,
                Owner(uuid),
                id,
                transform_at(0),
                TickedInterpolation::default(),
            ))
            .id()
    }

    /// Give every peer on the network a body, minted on the host. Returns `(uuid, tracked id)`
    /// per peer, in peer order.
    ///
    /// After [`adopt_roles`](TickedNetwork::adopt_roles): the frame a host takes its role it
    /// despawns whatever it built while solo, and a body minted before that is gone by the time
    /// anyone looks for it.
    pub fn seat_everyone(net: &mut TickedNetwork) -> Vec<(u128, u64)> {
        let host = net.host();
        let seats: Vec<(u128, u8)> = net
            .peers()
            .into_iter()
            .map(|peer| {
                let slot = net
                    .app(peer)
                    .world()
                    .get_resource::<LocalSpawnerSlot>()
                    .map_or(0, |slot| slot.0.0);
                (net.uuid(peer), slot)
            })
            .collect();
        let world = net.world_mut(host);
        seats
            .into_iter()
            .map(|(uuid, slot)| {
                let entity = spawn_player(world, uuid);
                world.entity_mut(entity).insert(PlayerSlot(slot));
                let id = world
                    .get::<TickTrackedEntity>(entity)
                    .expect("just spawned")
                    .0;
                (uuid, id)
            })
            .collect()
    }

    /// The world reduced to three numbers: how many bodies, where they are, how fast they move.
    ///
    /// Sections rather than one hash, so a divergence report can say *what* differed. Sorted by
    /// tracked id before folding, because a hash over query order is a hash over archetype
    /// order, which two peers do not share.
    #[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
    pub struct MinimalHash {
        pub bodies: u64,
        pub positions: u64,
        pub velocities: u64,
    }

    fn fold(acc: u64, value: u64) -> u64 {
        // FNV-1a over the eight bytes of the value.
        let mut hash = acc;
        for byte in value.to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        hash
    }

    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

    impl WorldHash for MinimalHash {
        fn sample(world: &mut World) -> Self {
            let mut rows: Vec<(u64, i64, i64)> = world
                .query::<(&TickTrackedEntity, &Pos, &Vel)>()
                .iter(world)
                .map(|(tracked, pos, vel)| (tracked.0, pos.0, vel.0))
                .collect();
            rows.sort_unstable();
            let mut bodies = FNV_OFFSET;
            let mut positions = FNV_OFFSET;
            let mut velocities = FNV_OFFSET;
            for (id, pos, vel) in rows {
                bodies = fold(bodies, id);
                positions = fold(fold(positions, id), pos as u64);
                velocities = fold(fold(velocities, id), vel as u64);
            }
            Self {
                bodies,
                positions,
                velocities,
            }
        }

        fn value(&self) -> u64 {
            self.bodies ^ self.positions.rotate_left(21) ^ self.velocities.rotate_left(42)
        }

        fn differences(&self, other: &Self) -> Vec<&'static str> {
            let mut sections = Vec::new();
            if self.bodies != other.bodies {
                sections.push("bodies");
            }
            if self.positions != other.positions {
                sections.push("positions");
            }
            if self.velocities != other.velocities {
                sections.push("velocities");
            }
            sections
        }
    }
}
