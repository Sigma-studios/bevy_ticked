//! Saying so, loudly, when a client stops hearing its host — and when a host stops being heard.
//!
//! # The failure this exists for
//!
//! A client whose host's snapshots stop being applied does not stop. Its clock runs, its own
//! body is predicted and answers the keys, and everything else goes quiet: remote bodies run on
//! the inputs the host last relayed until those are too old to hold, then stand still; whatever
//! only the authority advances — a round's countdown — never moves again. Nothing ends it. The
//! transport's keepalives are answered at the socket, so the liveness timeout never fires, and
//! every way a snapshot can be dropped on the way in is counted somewhere nobody is looking.
//!
//! A web client in run-2d sat in exactly that state, more than once, with nothing in its console
//! but warnings about something else. Every path in that could be reasoned about was tested and
//! recovered; whatever it was, it was not one of them. So this does not guess at a fix. It makes
//! the next one name itself.
//!
//! # The client: [`SnapshotWatchdog`]
//!
//! The host's word has arrived when a snapshot is applied, or when one repeats the tick already
//! applied — a host whose clock is held, by a pause, sends the same tick every half second, and
//! that is a host still talking. Frame time since then is counted, and once it passes
//! [`SnapshotWatchdogPolicy::starved_after`] the client is *starved*:
//!
//! - `error!`, once per episode, with every counter that tells the causes apart: how many
//!   packets arrived, and of those how many were stale, against a baseline this client no longer
//!   held, before the handshake, or not decodable at all; the newest tick that arrived against
//!   the newest applied; what holds the clock; the longest frame in the silence.
//! - A full body is asked for at once and again every [`SnapshotWatchdogPolicy::nack_every`],
//!   held clock or not: a client that lost its delta chain recovers from one keyframe.
//! - [`SnapshotWatchdog::starved_for`] says how long, for a game that would rather leave than
//!   wait.
//!
//! When the host's word comes back, a `warn!` says how long it was gone.
//!
//! Each frame counts for at most [`SnapshotWatchdogPolicy::max_frame_counted`]. A tab coming
//! back from the background delivers its whole absence as one frame, together with every packet
//! that arrived meanwhile; counted whole, that frame would be a starvation every time somebody
//! looked at another window.
//!
//! Counting starts at the first applied snapshot. Before it, the handshake and the initial sync
//! have their own timeouts, and a slow join is not this.
//!
//! # The host: [`AckWatch`]
//!
//! The same silence, from the other side: a client whose inputs keep arriving — so the link
//! from it works and it is playing — but whose acknowledgement has not moved for
//! [`SnapshotWatchdogPolicy::starved_after`]. `error!` once per episode, naming whether the
//! client is still a snapshot recipient and which `seq` was last sent and acknowledged. Not
//! while the host's clock is held: a held host repeats one tick, and a client acknowledges
//! nothing it has already applied.

use std::collections::HashMap;
use std::time::Duration;

use bevy::prelude::*;
use bevy_ticked::{
    session::{PerPeer, SessionAppExt, SessionReset, SessionScope},
    tick::{CurrentTick, TickHolds},
    tick_types::Tick,
};

use crate::client::{AppliedSnapshotTick, LocalClientPlayer, NackFull};
use crate::diagnostics::ReplayStats;
use crate::pause::SessionPause;
use crate::server::{LastAck, LocalServerPlayer, SnapshotRecipientList, SnapshotSeq};

/// How long a silence has to last before it is one, and what is done about it.
///
/// Shared by both sides. `init_resource`d, so a game's own, inserted first, stands.
#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotWatchdogPolicy {
    /// Frame time without the host's word, on a client — or without an acknowledgement from a
    /// client still sending inputs, on the host — before it is reported.
    pub starved_after: Duration,
    /// How often a starved client asks for a full body again.
    pub nack_every: Duration,
    /// The most one frame contributes to a silence. See the module docs.
    pub max_frame_counted: Duration,
}

impl Default for SnapshotWatchdogPolicy {
    fn default() -> Self {
        Self {
            // Snapshots come every tick. Two seconds is a hundred and twenty-eight of them lost
            // in a row, which no link that is still a link does.
            starved_after: Duration::from_secs(2),
            nack_every: Duration::from_secs(1),
            max_frame_counted: Duration::from_millis(250),
        }
    }
}

/// A client's count of how long it has gone without its host's word. See the module docs.
#[derive(Resource, Debug, Clone, Default)]
pub struct SnapshotWatchdog {
    /// Frame time since the host's word last arrived, each frame capped.
    silent_for: Duration,
    /// The newest tick applied when last looked at, to see a new one.
    last_applied: Option<Tick>,
    /// Set by the receiving observer: a snapshot repeated the applied tick.
    heard_repeat: bool,
    /// Set by [`excuse`](Self::excuse): the silence is expected, this frame.
    excused: bool,
    /// The newest tick any snapshot arrived for, applied or not.
    newest_arrived: Option<Tick>,
    /// The counters as they stood when the host was last heard, so a report is about the
    /// silence and not the session. `None` outside one: `ReplayStats` is cumulative and not
    /// session state, and a copy of it taken between sessions would outlive the door.
    stats_when_heard: Option<ReplayStats>,
    /// The longest frame since the host was last heard, uncapped.
    longest_frame: Duration,
    /// Starved, and since how long into the silence the last full body was asked for.
    starved: Option<Duration>,
}

impl SessionReset for SnapshotWatchdog {}

impl SnapshotWatchdog {
    /// How long this client has been starved, counted from the start of the silence; `None`
    /// while it is not.
    pub fn starved_for(&self) -> Option<Duration> {
        self.starved.map(|_| self.silent_for)
    }

    /// The silence is expected — a lobby waiting for a new host — and is not counted this frame.
    /// Call it every frame it holds; a starvation in progress ends without a report.
    pub fn excuse(&mut self) {
        self.excused = true;
    }

    /// A snapshot for `tick` arrived. `repeat`: it was for the tick already applied.
    pub(crate) fn arrived(&mut self, tick: Tick, repeat: bool) {
        self.newest_arrived = self.newest_arrived.max(Some(tick));
        self.heard_repeat |= repeat;
    }

    fn heard(&mut self, stats: Option<ReplayStats>) {
        self.silent_for = Duration::ZERO;
        self.stats_when_heard = stats;
        self.longest_frame = Duration::ZERO;
    }
}

pub(crate) fn install_client(app: &mut App) {
    app.init_resource::<SnapshotWatchdogPolicy>()
        .init_session_resource_scoped::<SnapshotWatchdog>(SessionScope::Role)
        .add_systems(PostUpdate, watch_for_starvation);
}

pub(crate) fn install_server(app: &mut App) {
    app.init_resource::<SnapshotWatchdogPolicy>()
        .init_per_peer_resource::<AckWatch>()
        .add_systems(PostUpdate, watch_for_unacknowledged);
}

#[allow(clippy::too_many_arguments)]
fn watch_for_starvation(
    time: Res<Time<Real>>,
    policy: Res<SnapshotWatchdogPolicy>,
    client: Option<Res<LocalClientPlayer>>,
    applied: Res<AppliedSnapshotTick>,
    tick: Res<CurrentTick>,
    holds: Res<TickHolds>,
    pause: Res<SessionPause>,
    stats: Res<ReplayStats>,
    mut watchdog: ResMut<SnapshotWatchdog>,
    mut nack: ResMut<NackFull>,
) {
    let fresh = applied.0 != watchdog.last_applied;
    watchdog.last_applied = applied.0;
    let repeat = std::mem::take(&mut watchdog.heard_repeat);
    let excused = std::mem::take(&mut watchdog.excused);

    // Not a client, or not yet synced: nothing to hear.
    if client.is_none() || applied.0.is_none() || excused {
        watchdog.starved = None;
        watchdog.heard(None);
        return;
    }
    if fresh || repeat {
        if watchdog.starved.is_some() {
            warn!(
                "snapshots from the host are being applied again, after {:.1?} without",
                watchdog.silent_for
            );
        }
        watchdog.starved = None;
        watchdog.heard(Some(*stats));
        return;
    }

    let frame = time.delta();
    watchdog.longest_frame = watchdog.longest_frame.max(frame);
    watchdog.silent_for += frame.min(policy.max_frame_counted);
    let silent_for = watchdog.silent_for;

    match watchdog.starved {
        None if silent_for >= policy.starved_after => {
            let since = stats.since(&watchdog.stats_when_heard.unwrap_or_default());
            let reasons: Vec<_> = holds.reasons().collect();
            error!(
                "no snapshot from the host has been applied for {silent_for:.1?}, and this client \
                 is predicting alone: everybody else will stand still, and nothing only the host \
                 decides will happen. Since the last one: {} arrived, newest for tick {} — {} \
                 stale, {} against a baseline this client no longer held, {} before the \
                 handshake, {} that did not decode. Newest applied tick {}, current tick {}. \
                 Clock holds {reasons:?}, session pause {:?}. Longest frame in the silence \
                 {:.1?}. Asking the host for a full snapshot.",
                since.snapshots_received,
                watchdog
                    .newest_arrived
                    .map_or_else(|| "none".to_string(), |tick| tick.to_string()),
                since.dropped_stale,
                since.dropped_unknown_baseline,
                since.dropped_before_handshake,
                since.dropped_undecodable,
                applied
                    .0
                    .map_or_else(|| "none".to_string(), |tick| tick.to_string()),
                tick.0,
                pause.0,
                watchdog.longest_frame,
            );
            nack.0 = true;
            watchdog.starved = Some(silent_for);
        }
        Some(last_nack) if silent_for.saturating_sub(last_nack) >= policy.nack_every => {
            nack.0 = true;
            watchdog.starved = Some(silent_for);
        }
        _ => {}
    }
}

/// The host's count, per client, of how long its acknowledgement has stood still while its
/// inputs kept coming. See the module docs.
#[derive(Resource, Default, Debug, Clone)]
pub struct AckWatch(HashMap<u128, ClientAckWatch>);

#[derive(Default, Debug, Clone)]
struct ClientAckWatch {
    /// The acknowledgement when last looked at.
    acked: Option<u32>,
    /// Frame time it has stood still, each frame capped.
    still_for: Duration,
    /// Frame time since an input from this client last arrived, each frame capped.
    since_input: Duration,
    /// Set by the input observer.
    input_heard: bool,
    reported: bool,
}

impl SessionReset for AckWatch {}
impl PerPeer for AckWatch {
    fn forget(&mut self, peer: u128) {
        self.0.remove(&peer);
    }
}

impl AckWatch {
    /// The clients reported as playing on without acknowledging, with how long for.
    pub fn unacknowledged(&self) -> impl Iterator<Item = (u128, Duration)> + '_ {
        self.0
            .iter()
            .filter(|(_, client)| client.reported)
            .map(|(uuid, client)| (*uuid, client.still_for))
    }

    /// An input from `sender` arrived.
    pub(crate) fn input_from(&mut self, sender: u128) {
        self.0.entry(sender).or_default().input_heard = true;
    }
}

#[allow(clippy::too_many_arguments)]
fn watch_for_unacknowledged(
    time: Res<Time<Real>>,
    policy: Res<SnapshotWatchdogPolicy>,
    host: Option<Res<LocalServerPlayer>>,
    holds: Res<TickHolds>,
    acks: Res<LastAck>,
    seqs: Res<SnapshotSeq>,
    recipients: Option<Res<SnapshotRecipientList>>,
    mut watch: ResMut<AckWatch>,
) {
    let frame = time.delta().min(policy.max_frame_counted);
    // A held host repeats its tick and a client acknowledges nothing new: not a silence.
    let counting = host.is_some() && !holds.is_held();
    for (uuid, client) in watch.0.iter_mut() {
        let acked = acks.0.get(uuid).copied();
        let input_heard = std::mem::take(&mut client.input_heard);
        if input_heard {
            client.since_input = Duration::ZERO;
        } else {
            client.since_input += frame;
        }
        if !counting || acked != client.acked {
            if client.reported && acked != client.acked {
                warn!(
                    "client {uuid:#x} is acknowledging snapshots again, after {:.1?} without",
                    client.still_for
                );
            }
            client.acked = acked;
            client.still_for = Duration::ZERO;
            client.reported = false;
            continue;
        }
        client.still_for += frame;
        // Inputs within the last half of the threshold: the client is there and playing.
        let playing = client.since_input < policy.starved_after / 2;
        if client.still_for >= policy.starved_after && playing && !client.reported {
            client.reported = true;
            let listed = recipients
                .as_deref()
                .map_or("the recipient list is not kept", |list| {
                    if list.0.contains(uuid) {
                        "it is a snapshot recipient"
                    } else {
                        "it is NOT a snapshot recipient"
                    }
                });
            error!(
                "client {uuid:#x} has acknowledged no snapshot for {:.1?} while its inputs keep \
                 arriving: it is playing on without the host's world. Last acknowledged seq \
                 {acked:?}, last sent {:?}; {listed}.",
                client.still_for,
                seqs.0.get(uuid),
            );
        }
    }
}
