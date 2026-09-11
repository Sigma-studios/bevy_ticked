//! N peers over the loopback backend, with the ticked stack's idea of what a session is.
//!
//! `bevy_ensemble_loopback::LoopbackNetwork` knows about peers, links and frames. It does not
//! know what a tick is, what "settled" means, or that a frame on one peer can be two ticks on
//! another. Every consumer wrote that layer on top of it — `adopt_roles`, `settle`, a freeze, a
//! way to read the trace as snapshots and inputs rather than bytes — and each wrote it slightly
//! wrong: a settle that checked roles but not the first snapshot, a lead read the wrong way
//! round, a "freeze" that stopped the network's clock instead of one peer's.
//!
//! [`TickedNetwork`] is that layer. The loopback network is a public field, so anything it does
//! that this does not wrap is one `.net` away.

use std::collections::BTreeSet;
use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ensemble::{EnsembleMessage, EnsembleMessageRegistry, packet_index, unframe_packet};
use bevy_ensemble_loopback::{Link, LoopbackNetwork, PacketFate, PeerId, SentPacket};
use bevy_ticked::tracked_entity::TickTrackedEntity;
use bevy_ticked_networking::input::TickedInput;
use bevy_ticked_networking::snapshot::{SnapshotPacket, decode_packet};
use bevy_ticked_networking_ensemble::{EnsembleInputMessage, EnsembleSnapshotMessage};

use crate::peer::{HOST_UUID, TICK, client_server_peer};
use crate::view::{applied_tick, role, tick};

/// What a peer is in the session: the authority, a follower, or neither.
///
/// Three-valued because the stack's is two booleans that are both false for a solo player, and
/// every game with a single-player mode wrote this enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    Host,
    Client,
    Solo,
}

/// One snapshot as it crossed the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotOnWire {
    /// The frame the sender handed it over.
    pub frame: u64,
    /// The whole packet, including any other messages framed with it.
    pub bytes: usize,
    pub delivered: bool,
}

/// One decoded message and when it travelled. See [`TickedNetwork::decode_messages_traced`].
#[derive(Clone, Debug)]
pub struct TracedMessage<M> {
    /// The frame the sender handed it over.
    pub sent_frame: u64,
    /// The frame it arrived, or `None` if the link lost it.
    pub arrived_frame: Option<u64>,
    pub message: M,
}

/// One input packet as it crossed the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputOnWire {
    pub frame: u64,
    pub bytes: usize,
    pub delivered: bool,
}

/// Builds one peer for a uuid: this crate's plugins, then the given build step.
type Stack = Box<dyn Fn(u128, &mut dyn FnMut(&mut App)) -> App>;

/// The game's own build step, run on every peer the network makes unless told otherwise.
type Build = Box<dyn Fn(&mut App)>;

/// One host, any number of clients, one clock, and the vocabulary of the stack on top.
pub struct TickedNetwork {
    /// The loopback backend underneath. Public: anything not wrapped here is a call on it.
    pub net: LoopbackNetwork,
    stack: Stack,
    base: Build,
    next_uuid: u128,
    /// Every tracked id seen on the host since [`record_issued_ids`](Self::record_issued_ids).
    issued: Option<BTreeSet<u64>>,
    /// Per peer, how many ticks one of its frames is worth; see
    /// [`set_ticks_per_frame`](Self::set_ticks_per_frame). Empty while everyone runs at the
    /// network's frame rate.
    cadence: Vec<(PeerId, u32, u32)>,
    /// Network frames stepped so far; the slow peers' phase.
    frame: u64,
    /// Peers whose next frame is a long one (after a freeze), and the frame to put back.
    clock_restore: Vec<(PeerId, Duration)>,
}

impl TickedNetwork {
    /// A host with uuid [`HOST_UUID`] and `clients` clients numbered from 2, every one of them a
    /// [`client_server_peer`] with `build` run on top.
    ///
    /// `build` is kept, so [`add_client`](Self::add_client) can make more of the same later.
    pub fn client_server<I: TickedInput>(
        clients: usize,
        build: impl Fn(&mut App) + 'static,
    ) -> Self {
        let stack: Stack = Box::new(move |uuid, extra| client_server_peer::<I>(uuid, extra));
        Self::from_stack(stack, Box::new(build), clients)
    }

    /// A lockstep session: every peer is a [`peer_app`](crate::peer::peer_app) with
    /// `LockstepPlugin<A, S>`, a per-tick `ChecksumLogPlugin<H>` and `ChecksumExchangePlugin<H>`
    /// added before `build`.
    #[cfg(feature = "lockstep")]
    pub fn lockstep<A, S, H>(
        clients: usize,
        config: bevy_ticked_lockstep_networking::LockstepConfig,
        build: impl Fn(&mut App) + 'static,
    ) -> Self
    where
        A: bevy_ticked_lockstep_networking::LockstepAction,
        S: bevy_ticked_lockstep_networking::JoinSnapshot,
        H: bevy_ticked::checksum::WorldHash + serde::Serialize + serde::de::DeserializeOwned,
    {
        let stack: Stack =
            Box::new(move |uuid, extra| lockstep_peer::<A, S, H>(uuid, config, extra));
        Self::from_stack(stack, Box::new(build), clients)
    }

    fn from_stack(stack: Stack, base: Build, clients: usize) -> Self {
        let host = stack(HOST_UUID, &mut |app| base(app));
        let mut net = LoopbackNetwork::new(TICK);
        net.add_host(HOST_UUID, host);
        let mut this = Self {
            net,
            stack,
            base,
            next_uuid: HOST_UUID + 1,
            issued: None,
            cadence: Vec::new(),
            frame: 0,
            clock_restore: Vec::new(),
        };
        for _ in 0..clients {
            this.add_client();
        }
        this
    }

    /// The default link for every pair. Takes effect on the next step.
    pub fn with_link(mut self, link: Link) -> Self {
        self.net.set_link(link);
        self
    }

    /// Pick the run: the same seed, peers and inputs replay the same trace.
    pub fn with_seed(mut self, seed: u64) -> Self {
        self.net.seed(seed);
        self
    }

    // ---- peers -----------------------------------------------------------------------------

    /// This crate's plugins, the network's usual build if `with_base`, then `build`.
    fn build_peer(&mut self, uuid: u128, with_base: bool, build: impl FnOnce(&mut App)) -> App {
        let mut build = Some(build);
        let base = &self.base;
        (self.stack)(uuid, &mut |app| {
            if with_base {
                base(app);
            }
            if let Some(build) = build.take() {
                build(app);
            }
        })
    }

    fn mint_uuid(&mut self) -> u128 {
        let uuid = self.next_uuid;
        self.next_uuid += 1;
        uuid
    }

    /// Attach another client built like the others, with the next uuid.
    pub fn add_client(&mut self) -> PeerId {
        self.add_client_with(|_| {})
    }

    /// Attach a client built like the others, then `build` on top — an extra plugin, a
    /// resource preset, a different history window.
    pub fn add_client_with(&mut self, build: impl FnOnce(&mut App)) -> PeerId {
        let uuid = self.mint_uuid();
        let app = self.build_peer(uuid, true, build);
        self.net.add_client(uuid, app)
    }

    /// Attach a client built with this crate's plugins and `build` **instead of** the network's
    /// usual build: a peer that registers one component fewer, or in another order, the way a
    /// peer from a different commit would. [`add_client_with`](Self::add_client_with) cannot
    /// take a registration away, only add one.
    pub fn add_client_built(&mut self, build: impl FnOnce(&mut App)) -> PeerId {
        let uuid = self.mint_uuid();
        let app = self.build_peer(uuid, false, build);
        self.net.add_client(uuid, app)
    }

    /// Attach a client whose join is not finished: packets flow, the roster does not know it.
    /// [`promote`](Self::promote) finishes the join.
    pub fn add_pending_client(&mut self) -> PeerId {
        let uuid = self.mint_uuid();
        let app = self.build_peer(uuid, true, |_| {});
        self.net.add_pending_client(uuid, app)
    }

    pub fn promote(&mut self, peer: PeerId) {
        self.net.promote(peer);
    }

    pub fn leave(&mut self, peer: PeerId) {
        self.net.leave(peer);
    }

    pub fn rejoin(&mut self, peer: PeerId) {
        self.net.rejoin(peer);
    }

    pub fn rehost(&mut self, new_host: PeerId) {
        self.net.rehost(new_host);
    }

    pub fn disconnect(&mut self, peer: PeerId) {
        self.net.disconnect(peer);
    }

    pub fn half_open(&mut self, peer: PeerId) {
        self.net.half_open(peer);
    }

    pub fn reconnect(&mut self, peer: PeerId) {
        self.net.reconnect(peer);
    }

    // ---- looking around --------------------------------------------------------------------

    pub fn host(&self) -> PeerId {
        self.net.host()
    }

    /// The first client. For the two-peer tests that are most of them.
    ///
    /// # Panics
    ///
    /// If there is none.
    pub fn client(&self) -> PeerId {
        self.clients()
            .into_iter()
            .next()
            .expect("this network has no client")
    }

    /// Every peer that is not the host, in peer order.
    pub fn clients(&self) -> Vec<PeerId> {
        let host = self.net.host();
        self.net.peers().filter(|peer| *peer != host).collect()
    }

    pub fn peers(&self) -> Vec<PeerId> {
        self.net.peers().collect()
    }

    /// Peers that are on the network as far as the session is concerned: connected or pending.
    /// Half-open, disconnected and departed peers are left out of "everyone".
    fn attached(&self) -> Vec<PeerId> {
        self.net
            .peers()
            .filter(|peer| self.net.is_connected(*peer) || self.net.is_pending(*peer))
            .collect()
    }

    pub fn uuid(&self, peer: PeerId) -> u128 {
        self.net.uuid(peer)
    }

    pub fn peer_by_uuid(&self, uuid: u128) -> Option<PeerId> {
        self.net.peer_by_uuid(uuid)
    }

    pub fn app(&self, peer: PeerId) -> &App {
        self.net.app(peer)
    }

    pub fn app_mut(&mut self, peer: PeerId) -> &mut App {
        self.net.app_mut(peer)
    }

    pub fn world_mut(&mut self, peer: PeerId) -> &mut World {
        self.net.app_mut(peer).world_mut()
    }

    // ---- the frame loop --------------------------------------------------------------------

    /// One frame on every peer.
    ///
    /// Wall time is the network's frame for everyone; a peer with a cadence set runs fewer or
    /// more updates in it, never a longer or shorter frame than the clock says.
    pub fn step(&mut self) {
        let restore = std::mem::take(&mut self.clock_restore);
        if self.cadence.is_empty() {
            self.net.step();
        } else {
            let frame = self.frame;
            let cadence = self.cadence.clone();
            self.net.step_with(|peer, app| {
                let updates = match cadence.iter().find(|(who, _, _)| *who == peer) {
                    // `every` frames, this peer runs once.
                    Some((_, every, 1)) => u32::from(frame.is_multiple_of(u64::from(*every))),
                    // Every frame, this peer runs `times` times.
                    Some((_, 1, times)) => *times,
                    Some((_, every, times)) => {
                        if frame.is_multiple_of(u64::from(*every)) {
                            *times
                        } else {
                            0
                        }
                    }
                    None => 1,
                };
                for _ in 0..updates {
                    app.update();
                }
            });
        }
        self.frame += 1;
        for (peer, frame) in restore {
            self.net
                .app_mut(peer)
                .insert_resource(TimeUpdateStrategy::ManualDuration(frame));
        }
        self.after_frame();
    }

    pub fn run(&mut self, frames: usize) {
        for _ in 0..frames {
            self.step();
        }
    }

    /// Step until `condition` holds, up to `max_frames`. Returns whether it held.
    pub fn run_until(&mut self, max_frames: usize, condition: impl Fn(&Self) -> bool) -> bool {
        for _ in 0..max_frames {
            if condition(self) {
                return true;
            }
            self.step();
        }
        condition(self)
    }

    /// Step until `peer` has simulated `tick`, up to `max_frames`.
    pub fn run_until_tick(&mut self, peer: PeerId, target: u64, max_frames: usize) -> bool {
        self.run_until(max_frames, |net| tick(net.app(peer)) >= target)
    }

    /// Step until every attached peer holds a role. A host that has not adopted yet is not
    /// broadcasting, and a client that has not is dropping every snapshot at the door.
    pub fn adopt_roles(&mut self, max_frames: usize) -> bool {
        self.run_until(max_frames, |net| {
            net.attached()
                .into_iter()
                .all(|peer| role(net.app(peer)) != Role::Solo)
        })
    }

    /// Step until the session is running: roles adopted, every attached client has applied a
    /// snapshot, and every one of them leads the host.
    ///
    /// The third condition is the one a settle check tends to leave out, and it is the one that
    /// matters: a client that has applied a snapshot and does *not* lead has every input it will
    /// ever send arrive too late to be read.
    pub fn settle(&mut self, max_frames: usize) -> bool {
        self.run_until(max_frames, |net| {
            let host = net.host();
            let host_tick = tick(net.app(host));
            net.attached().into_iter().all(|peer| {
                let app = net.app(peer);
                match role(app) {
                    Role::Host => true,
                    Role::Client => applied_tick(app).is_some() && tick(app) > host_tick,
                    Role::Solo => false,
                }
            })
        })
    }

    /// `frames` frames in which `peer` does not run: no update, nothing sent, and what arrives
    /// for it waits. A backgrounded tab, from the outside.
    pub fn freeze(&mut self, peer: PeerId, frames: usize) {
        let others: Vec<PeerId> = self.net.peers().filter(|other| *other != peer).collect();
        for _ in 0..frames {
            self.net.step_only(&others);
            self.after_frame();
        }
        // The frozen peer's next frame is as long as the freeze: that is what a real clock
        // reports after a tab switch, and what the host's auto-pause looks for.
        let frame = self.frame_of(peer);
        self.net
            .app_mut(peer)
            .insert_resource(TimeUpdateStrategy::ManualDuration(frame * frames as u32));
        self.clock_restore.push((peer, frame));
    }

    /// One frame that is `ticks` ticks long on every peer, then back to whatever each had.
    ///
    /// A long frame — a shader compile, a collection, a window drag — is what makes a client
    /// fall behind, and the recovery from it is a path that only runs if a test can produce one.
    /// Peers are expected to be on `TimeUpdateStrategy::ManualDuration`, which
    /// [`peer_app_with`](crate::peer::peer_app_with) guarantees.
    pub fn step_frame_with_ticks(&mut self, ticks: u32) {
        let peers = self.peers();
        let saved: Vec<(PeerId, Duration)> = peers
            .iter()
            .map(|peer| (*peer, self.frame_of(*peer)))
            .collect();
        for peer in &peers {
            self.net
                .app_mut(*peer)
                .insert_resource(TimeUpdateStrategy::ManualDuration(TICK * ticks));
        }
        self.step();
        for (peer, frame) in saved {
            self.net
                .app_mut(peer)
                .insert_resource(TimeUpdateStrategy::ManualDuration(frame));
        }
    }

    /// From now on `peer` renders at a different rate: one of its frames is `ratio` ticks of
    /// virtual time. `2.0` is a 32 fps peer on a 64 Hz network (it updates every other network
    /// frame, each update two ticks long); `0.5` is a 128 fps peer (two updates per network
    /// frame, half a tick each). `1.0` puts it back.
    ///
    /// Wall time stays the network's: a slow peer does not fall behind the clock and a fast one
    /// does not run ahead of it, which is the difference between a frame rate and a clock speed.
    /// Only ratios that are a whole number or the reciprocal of one are representable.
    pub fn set_ticks_per_frame(&mut self, peer: PeerId, ratio: f32) {
        self.cadence.retain(|(who, _, _)| *who != peer);
        let (every, times) = if ratio >= 1.0 {
            assert!(
                (ratio - ratio.round()).abs() < 1e-6,
                "a slow peer's ratio must be a whole number of network frames, not {ratio}"
            );
            (ratio.round() as u32, 1)
        } else {
            let times = 1.0 / ratio;
            assert!(
                (times - times.round()).abs() < 1e-6,
                "a fast peer's ratio must be the reciprocal of a whole number, not {ratio}"
            );
            (1, times.round() as u32)
        };
        if (every, times) != (1, 1) {
            self.cadence.push((peer, every, times));
        }
        self.net
            .app_mut(peer)
            .insert_resource(TimeUpdateStrategy::ManualDuration(TICK.mul_f32(ratio)));
    }

    fn frame_of(&self, peer: PeerId) -> Duration {
        match self
            .net
            .app(peer)
            .world()
            .get_resource::<TimeUpdateStrategy>()
        {
            Some(TimeUpdateStrategy::ManualDuration(frame)) => *frame,
            _ => TICK,
        }
    }

    // ---- the wire --------------------------------------------------------------------------

    /// Start recording every packet. Off by default; the record grows while on.
    pub fn trace_packets(&mut self) {
        self.net.trace_packets(true);
    }

    pub fn trace(&self) -> &[SentPacket] {
        self.net.trace()
    }

    pub fn take_trace(&mut self) -> Vec<SentPacket> {
        self.net.take_trace()
    }

    /// Bytes handed to the network from `from` to `to` since creation, delivered or not. Counted
    /// whether or not tracing is on.
    pub fn bytes_sent(&self, from: PeerId, to: PeerId) -> u64 {
        self.net.bytes_sent(from, to)
    }

    /// The wire index `T` travels under, as the sender's registry has it.
    fn wire_index<T: EnsembleMessage>(&self, sender: PeerId) -> Option<u16> {
        self.net
            .app(sender)
            .world()
            .get_resource::<EnsembleMessageRegistry>()?
            .index_of::<T>()
    }

    /// Traced packets from `from` to `to` carrying a message with wire index `index`, framed
    /// with others or alone. A frame that holds one counts once.
    fn packets_carrying(&self, from: PeerId, to: PeerId, index: u16) -> Vec<&SentPacket> {
        self.trace()
            .iter()
            .filter(|packet| packet.from == from && packet.to == to)
            .filter(|packet| carries(&packet.bytes, index))
            .collect()
    }

    /// Every traced packet from `from` to `to` that carried a snapshot.
    pub fn snapshot_packets(&self, from: PeerId, to: PeerId) -> Vec<SentPacket> {
        self.wire_index::<EnsembleSnapshotMessage>(from)
            .map(|index| {
                self.packets_carrying(from, to, index)
                    .into_iter()
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Every traced packet from `from` to `to` that carried an `I` input.
    pub fn input_packets<I: TickedInput>(&self, from: PeerId, to: PeerId) -> Vec<SentPacket> {
        self.wire_index::<EnsembleInputMessage<I>>(from)
            .map(|index| {
                self.packets_carrying(from, to, index)
                    .into_iter()
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The snapshots traced from `from` to `to`, by frame and size. Needs
    /// [`trace_packets`](Self::trace_packets) on.
    pub fn snapshots_sent(&self, from: PeerId, to: PeerId) -> Vec<SnapshotOnWire> {
        self.snapshot_packets(from, to)
            .into_iter()
            .map(|packet| SnapshotOnWire {
                frame: packet.frame,
                bytes: packet.bytes.len(),
                delivered: packet.was_delivered(),
            })
            .collect()
    }

    /// Every snapshot traced from `from` to `to`, decoded, in send order — delivered or not.
    ///
    /// Three envelopes come off: the loopback frame (several messages packed into one
    /// datagram), the ensemble message (a two-byte wire index and a postcard
    /// [`EnsembleSnapshotMessage`]), and the packet's own encoding. What is left is what the
    /// server built for that recipient: its `seq`, its `your_margin`, the body. A test that
    /// asks "what was this client told" reads these rather than the client's world, which has
    /// already predicted past them.
    ///
    /// A traced message that does not decode is skipped: a test corrupting packets on purpose
    /// wants to see what survived, not a panic in the harness.
    pub fn decode_snapshots(&self, from: PeerId, to: PeerId) -> Vec<SnapshotPacket> {
        let Some(index) = self.wire_index::<EnsembleSnapshotMessage>(from) else {
            return Vec::new();
        };
        self.trace()
            .iter()
            .filter(|packet| packet.from == from && packet.to == to)
            .flat_map(|packet| messages_in(&packet.bytes))
            .filter(|message| packet_index(message) == Some(index))
            .filter_map(|message| {
                let envelope: EnsembleSnapshotMessage =
                    postcard::from_bytes(&message[WIRE_INDEX_BYTES..]).ok()?;
                decode_packet(&envelope.bytes)
            })
            .collect()
    }

    /// Every traced message of type `M` from `from` to `to`, decoded, in send order, delivered
    /// or not. The generic form of [`decode_snapshots`](Self::decode_snapshots): a test reading
    /// what rode an input packet (its acknowledgement, say) decodes the bridge's input message.
    pub fn decode_messages<M: EnsembleMessage + serde::de::DeserializeOwned>(
        &self,
        from: PeerId,
        to: PeerId,
    ) -> Vec<M> {
        let Some(index) = self.wire_index::<M>(from) else {
            return Vec::new();
        };
        self.trace()
            .iter()
            .filter(|packet| packet.from == from && packet.to == to)
            .flat_map(|packet| messages_in(&packet.bytes))
            .filter(|message| packet_index(message) == Some(index))
            .filter_map(|message| postcard::from_bytes(&message[WIRE_INDEX_BYTES..]).ok())
            .collect()
    }

    /// As [`decode_messages`](Self::decode_messages), with the frame each message was handed
    /// over on and the frame it arrived, if it did. For a test that asks whether the sender
    /// could have known something when it sent: "was this baseline acknowledged before the
    /// delta against it went out".
    pub fn decode_messages_traced<M: EnsembleMessage + serde::de::DeserializeOwned>(
        &self,
        from: PeerId,
        to: PeerId,
    ) -> Vec<TracedMessage<M>> {
        let Some(index) = self.wire_index::<M>(from) else {
            return Vec::new();
        };
        self.trace()
            .iter()
            .filter(|packet| packet.from == from && packet.to == to)
            .flat_map(|packet| {
                let arrived = match packet.fate {
                    PacketFate::Delivered { at_frame } => Some(at_frame),
                    PacketFate::Duplicated { at_frames } => Some(at_frames[0]),
                    _ => None,
                };
                messages_in(&packet.bytes)
                    .into_iter()
                    .filter(move |message| packet_index(message) == Some(index))
                    .filter_map(move |message| {
                        let message: M = postcard::from_bytes(&message[WIRE_INDEX_BYTES..]).ok()?;
                        Some(TracedMessage {
                            sent_frame: packet.frame,
                            arrived_frame: arrived,
                            message,
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// The input packets traced from `from` to `to`, by frame and size.
    pub fn inputs_sent<I: TickedInput>(&self, from: PeerId, to: PeerId) -> Vec<InputOnWire> {
        self.input_packets::<I>(from, to)
            .into_iter()
            .map(|packet| InputOnWire {
                frame: packet.frame,
                bytes: packet.bytes.len(),
                delivered: packet.was_delivered(),
            })
            .collect()
    }

    // ---- id bookkeeping --------------------------------------------------------------------

    /// From now on, note every tracked id the host holds after each frame, so
    /// [`assert_no_id_unissued`](crate::assert::assert_no_id_unissued) can tell an id the host
    /// handed out from one a client made up.
    ///
    /// A set rather than a counter high-water mark, because a client's own counter is reset to
    /// the snapshot's maximum on every apply — so a client-minted id is usually *below* the
    /// host's high-water mark and a threshold would never see it.
    pub fn record_issued_ids(&mut self) {
        self.issued = Some(BTreeSet::new());
        self.after_frame();
    }

    /// The ids recorded so far, or `None` if recording was never switched on.
    pub fn issued_ids(&self) -> Option<&BTreeSet<u64>> {
        self.issued.as_ref()
    }

    fn after_frame(&mut self) {
        let Some(issued) = &mut self.issued else {
            return;
        };
        let host = self.net.host();
        let world = self.net.app_mut(host).world_mut();
        let mut tracked = world.query::<&TickTrackedEntity>();
        issued.extend(tracked.iter(world).map(|tracked| tracked.0));
    }
}

/// The two-byte wire index `bevy_ensemble` puts in front of every message's postcard body.
const WIRE_INDEX_BYTES: usize = 2;

/// The messages in `packet`: the ones a frame holds, or the packet itself when it is one.
fn messages_in(packet: &[u8]) -> Vec<&[u8]> {
    unframe_packet(packet).unwrap_or_else(|| vec![packet])
}

/// Whether `packet` — one message, or a frame of several — holds a message with `index`.
fn carries(packet: &[u8], index: u16) -> bool {
    messages_in(packet)
        .iter()
        .any(|message| packet_index(message) == Some(index))
}

/// A lockstep peer: [`peer_app`](crate::peer::peer_app) plus `LockstepPlugin<A, S>`, a per-tick
/// `ChecksumLogPlugin<H>` and `ChecksumExchangePlugin<H>`, added before `build`.
#[cfg(feature = "lockstep")]
pub fn lockstep_peer<A, S, H>(
    uuid: u128,
    config: bevy_ticked_lockstep_networking::LockstepConfig,
    build: impl FnOnce(&mut App),
) -> App
where
    A: bevy_ticked_lockstep_networking::LockstepAction,
    S: bevy_ticked_lockstep_networking::JoinSnapshot,
    H: bevy_ticked::checksum::WorldHash + serde::Serialize + serde::de::DeserializeOwned,
{
    use bevy_ticked::checksum::{ChecksumLog, ChecksumLogPlugin};
    use bevy_ticked_lockstep_networking::{ChecksumExchangePlugin, LockstepPlugin};

    crate::peer::peer_app(uuid, |app| {
        app.insert_resource(ChecksumLog::<H>::every_tick());
        app.add_plugins((
            LockstepPlugin::<A, S> {
                config,
                marker: std::marker::PhantomData,
            },
            ChecksumLogPlugin::<H>::default(),
            ChecksumExchangePlugin::<H>::default(),
        ));
        build(app);
    })
}
