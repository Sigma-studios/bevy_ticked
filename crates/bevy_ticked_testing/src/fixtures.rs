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
    use bevy_ticked::checksum::WorldHash;
    use bevy_ticked::tick::CurrentTick;
    use bevy_ticked::tracked_entity::{TickTrackedEntity, TickTrackedEntityCounter};
    use bevy_ticked::TickedSimulation;
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
    }

    /// Which player drives this body. Networked, so a client knows which body is its own.
    #[derive(Component, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Debug)]
    pub struct Owner(pub u128);

    /// What a player presses: a direction, or nothing.
    #[derive(Clone, Copy, Default, Serialize, Deserialize, Debug, PartialEq, Eq)]
    pub struct Input {
        pub dx: i8,
    }

    impl Input {
        pub const RIGHT: Self = Self { dx: 1 };
        pub const LEFT: Self = Self { dx: -1 };
        pub const NONE: Self = Self { dx: 0 };
    }

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

    /// Set every body's velocity from its owner's input for this tick, when there is one. A tick
    /// with no input for a player leaves the last velocity standing — a held key.
    pub fn apply_inputs(
        tick: Res<CurrentTick>,
        queue: Res<InputQueue<Input>>,
        mut bodies: Query<(&Owner, &mut Vel)>,
    ) {
        let Some(inputs) = queue.at_tick(tick.0) else {
            return;
        };
        for (owner, mut vel) in &mut bodies {
            if let Some(input) = inputs.get(&owner.0) {
                vel.0 = i64::from(input.dx);
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

    /// Mint a body for `uuid` from this world's counter. On a host, or on a solo peer; a client
    /// doing this is the bug `assert_no_id_unissued` exists to catch.
    pub fn spawn_player(world: &mut World, uuid: u128) -> Entity {
        let id = world.resource_mut::<TickTrackedEntityCounter>().next();
        world
            .spawn((Pos(0), Vel(0), EntityKind::PLAYER, Owner(uuid), id))
            .id()
    }

    /// Register the four networked components under stable wire names.
    ///
    /// Separate from [`install_systems`] so a test can build a peer that registers a different
    /// set — the way a peer built from another commit would — and watch the harness notice.
    pub fn register_components(app: &mut App) {
        app.register_networked_ticked_component_as::<Pos>("Pos")
            .register_networked_ticked_component_as::<Vel>("Vel")
            .register_networked_ticked_component_as::<EntityKind>("EntityKind")
            .register_networked_ticked_component_as::<Owner>("Owner");
    }

    /// The simulation systems and the `Visual` observer, without any registration.
    pub fn install_systems(app: &mut App) {
        app.configure_sets(
            TickedSimulation,
            (MinimalSet::Simulate, MinimalSet::Sample).chain(),
        )
        .add_systems(
            TickedSimulation,
            (apply_inputs, integrate)
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

    /// Give every peer on the network a body, minted on the host. Returns `(uuid, tracked id)`
    /// per peer, in peer order.
    ///
    /// After [`adopt_roles`](TickedNetwork::adopt_roles): the frame a host takes its role it
    /// despawns whatever it built while solo, and a body minted before that is gone by the time
    /// anyone looks for it.
    pub fn seat_everyone(net: &mut TickedNetwork) -> Vec<(u128, u64)> {
        let host = net.host();
        let uuids: Vec<u128> = net.peers().into_iter().map(|peer| net.uuid(peer)).collect();
        let world = net.world_mut(host);
        uuids
            .into_iter()
            .map(|uuid| {
                let entity = spawn_player(world, uuid);
                let id = world.get::<TickTrackedEntity>(entity).expect("just spawned").0;
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
