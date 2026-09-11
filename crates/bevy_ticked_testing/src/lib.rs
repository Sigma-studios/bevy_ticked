//! Testing a `bevy_ticked` game the way it is played: several peers, one process, a clock the
//! test controls, a link that can be told to misbehave, and assertions that speak the vocabulary
//! of the thing under test — ticks, leads, replays, snapshots, checksums — rather than of Bevy.
//!
//! Every consumer of `bevy_ticked` had written some of this. Each of them wrote the peer recipe
//! (the four non-obvious lines that make a headless peer behave), a way to queue input for the
//! tick about to run, a way to freeze a peer, a source-grep guardrail, a golden-trace helper. This
//! crate is those, once, plus the pieces none of them had: agreement across peers at the same
//! tick, replay purity, bandwidth and correction budgets, fault injection, and a harness that
//! proves it bites before anything is asserted with it.
//!
//! # Layers
//!
//! - [`peer`]: build one headless peer app that behaves like a shipped one.
//! - [`net`]: [`TickedNetwork`](net::TickedNetwork), N peers over the loopback backend, with the
//!   ticked stack's idea of settling, freezing, and frames that are not ticks.
//! - [`view`]: free functions over `&App` that read what a peer thinks: its tick, lead, replay
//!   counters, tracked ids, held input.
//! - [`input`]: queue input for the tick about to run; script actions by frame and player.
//! - [`assert`]: agreement, replay purity, id invariants, byte and correction budgets, log
//!   hygiene.
//! - [`measure`]: tick rate, input latency, tick cost, snapshot size.
//! - [`fault`]: corrupt a component, drop or corrupt a packet, deliver raw bytes, fuzz.
//! - [`source_guard`]: the grep-the-source guardrail, with a default needle list for a ticked
//!   simulation and a self-test that it bites.
//! - [`wire`]: pin a registry's shape.
//! - [`golden`]: record a trace and compare it to a file.
//! - [`log`]: capture warnings per test.
//! - [`fixtures`]: a minimal simulation to run all of the above against.

pub mod assert;
pub mod fault;
pub mod fixtures;
pub mod golden;
pub mod input;
pub mod log;
pub mod measure;
pub mod net;
pub mod peer;
pub mod source_guard;
pub mod view;
pub mod wire;

pub mod prelude {
    pub use crate::assert::*;
    pub use crate::fault::*;
    pub use crate::input::*;
    pub use crate::measure::*;
    pub use crate::net::{Role, TickedNetwork};
    pub use crate::peer::{HOST_UUID, PeerRecipe, TICK, client_server_peer, peer_app, peer_app_with};
    pub use crate::view::*;
    pub use bevy_ensemble_loopback::{Link, PacketFate, PeerId, SentPacket};
}
