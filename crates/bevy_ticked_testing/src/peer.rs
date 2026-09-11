//! One headless peer that behaves like a shipped one.
//!
//! Every consumer of this stack wrote this function, and each of them lost a day to one of the
//! four lines in it that are not obvious:
//!
//! - **`TimeUpdateStrategy::ManualDuration`.** Without it `Time` is driven by the wall clock, so
//!   a test's frame is however long the test took to run, the tick accumulator sees a different
//!   delta every run, and "one step is one tick" is true on a fast machine and false in CI.
//! - **`TickSource::Hz`.** The client steers its prediction lead by dilating the tick rate, and
//!   only the `Hz` source owns an accumulator to dilate. Under `FixedUpdate` it falls back to
//!   adding and dropping whole ticks, which is not what ships.
//! - **`DiagnosticsPlugin` and `TransformPlugin`.** Neither is in `MinimalPlugins`. Avian's
//!   diagnostics systems panic without the first; anything with a `Transform` hierarchy needs the
//!   second. They cost nothing on a fixture that uses neither.
//! - **`app.finish(); app.cleanup();`.** `App::update` does not finish plugins. Avian, among
//!   others, inserts resources in `finish`, and a peer that skips it panics on the first tick
//!   with a message about a resource that "should have been" there.
//!
//! [`peer_app_with`] is those four lines, once. [`client_server_peer`] adds the networking stack
//! in the order a game would.

use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ensemble::{EnsemblePlugin, LocalMultiplayerPlayerId, NetSimPlugin};
use bevy_ensemble_loopback::LoopbackTransportPlugin;
use bevy_ticked::tick::HistoryBufferTicks;
use bevy_ticked::{TickSource, TickedPlugin};
use bevy_ticked_networking::client::TickedClientPlugin;
use bevy_ticked_networking::input::TickedInput;
use bevy_ticked_networking::server::TickedServerPlugin;
use bevy_ticked_networking_ensemble::{TickedEnsembleSessionPlugin, TickedNetworkingEnsemblePlugin};

/// One tick at 64 Hz, exactly: 15.625 ms is representable, so a frame of this length is one
/// tick with no accumulator remainder and no drift.
///
/// Every duration in this crate — the network's frame, the peers' manual time step, the link's
/// delays — is in these units, so `Link::four_g()` at 40 ms reads as two and a half ticks.
pub const TICK: Duration = Duration::from_micros(15_625);

/// The uuid every network built here gives its host. Clients are numbered from 2.
pub const HOST_UUID: u128 = 1;

/// What one peer is made of, where the defaults are not the only sensible choice.
#[derive(Clone, Debug)]
pub struct PeerRecipe {
    /// The identity a real backend would learn from its signalling server.
    pub uuid: u128,
    /// What advances the tick clock. Defaults to `Hz(64.0)`, the only source a networked client
    /// can steer its lead under.
    pub source: TickSource,
    /// How much virtual time one `App::update` represents. Defaults to [`TICK`].
    pub frame: Duration,
    /// Override the history window in ticks; `None` leaves it to the plugins.
    pub history: Option<u64>,
    /// Add `bevy_ensemble::NetSimPlugin`, so `LoopbackNetwork::use_netsim` has something to
    /// drive. Off by default: the loopback `Link` impairs on the send side and is what most
    /// tests want.
    pub netsim: bool,
}

impl Default for PeerRecipe {
    fn default() -> Self {
        Self {
            uuid: HOST_UUID,
            source: TickSource::Hz(64.0),
            frame: TICK,
            history: None,
            netsim: false,
        }
    }
}

impl PeerRecipe {
    /// The defaults with this identity.
    pub fn for_uuid(uuid: u128) -> Self {
        Self {
            uuid,
            ..Default::default()
        }
    }
}

/// A headless peer: `MinimalPlugins`, transforms, diagnostics, a manual clock, the tick loop,
/// the lobby crate and the loopback transport — then `build`, then `finish` and `cleanup`.
///
/// `build` runs after this crate's plugins and before `finish`, which is where a game's own
/// plugins go. Registrations made there land in the same order a game makes them.
pub fn peer_app_with(recipe: PeerRecipe, build: impl FnOnce(&mut App)) -> App {
    crate::log::install();

    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        TransformPlugin,
        // Not in `MinimalPlugins`, and avian's diagnostics systems panic without the resources
        // it registers.
        bevy::diagnostic::DiagnosticsPlugin,
    ));
    // The clock is the test's, not the wall's: one frame is exactly `recipe.frame` of virtual
    // time, so the tick accumulator sees the same delta on every run and every machine.
    app.insert_resource(TimeUpdateStrategy::ManualDuration(recipe.frame));
    app.add_plugins(TickedPlugin {
        source: recipe.source,
        ..default()
    });
    app.add_plugins((EnsemblePlugin, LoopbackTransportPlugin));
    if recipe.netsim {
        app.add_plugins(NetSimPlugin);
    }
    // A real backend inserts this when the signalling server says which player you are. The
    // loopback backend does not model a signalling server, so the recipe plays that part — and it
    // is what `TickedEnsembleSessionPlugin::adopt_role` keys off.
    app.insert_resource(LocalMultiplayerPlayerId(recipe.uuid));
    if let Some(history) = recipe.history {
        app.insert_resource(HistoryBufferTicks(history));
    }

    build(&mut app);

    // `App::update` does not run plugin `finish`/`cleanup`; avian and others insert resources in
    // both, and a peer that skips them panics on its first tick.
    app.finish();
    app.cleanup();
    app
}

/// [`peer_app_with`] with the default recipe and this identity.
pub fn peer_app(uuid: u128, build: impl FnOnce(&mut App)) -> App {
    peer_app_with(PeerRecipe::for_uuid(uuid), build)
}

/// A peer that can host or join a client/server session: [`peer_app`] plus both role plugins,
/// the ensemble bridge and the session plugin, added **before** `build` so the game's own
/// registrations happen after the plugins, as they would in a game.
///
/// Both roles on every peer, because that is what a game ships: the same binary hosts or joins
/// depending on what the lobby says, and a harness that built host-only and client-only apps
/// would never see the resource collisions a peer holding both plugins can have.
pub fn client_server_peer<I: TickedInput>(uuid: u128, build: impl FnOnce(&mut App)) -> App {
    client_server_peer_with::<I>(PeerRecipe::for_uuid(uuid), build)
}

/// [`client_server_peer`] with a recipe.
pub fn client_server_peer_with<I: TickedInput>(
    recipe: PeerRecipe,
    build: impl FnOnce(&mut App),
) -> App {
    peer_app_with(recipe, |app| {
        app.add_plugins((
            TickedServerPlugin::<I>::new(),
            TickedClientPlugin::<I>::new(),
            TickedNetworkingEnsemblePlugin::<I>::new(),
            TickedEnsembleSessionPlugin,
        ));
        build(app);
    })
}
