//! Getting a divergence report to a peer that is actually playing.
//!
//! # The half that was missing
//!
//! [`checksum`](crate::checksum) moved the log and the divergence search into this crate on the
//! grounds that every lockstep game needs exactly them and none of it mentions any particular
//! game. That was right, and it stopped one step short of being useful.
//!
//! [`ChecksumLog::first_divergence`] takes **two logs**:
//!
//! ```ignore
//! pub fn first_divergence(&self, other: &ChecksumLog<H>) -> Option<Divergence<H>>
//! ```
//!
//! Two logs exist in one process in a test harness and in a multi-process tool that collects
//! both. They never exist in one process during a game. So a shipped peer could compute the
//! number, keep a bounded history of it, and search that history — against nothing. The
//! detection was upstream and the *delivery* was nowhere, which reads as "desync detection
//! exists" right up until the evening somebody needs it.
//!
//! This is the delivery. It carries the same argument as the log: "broadcast a hash on a
//! cadence, compare it against the local history, report the first tick that differed" contains
//! no game.
//!
//! # The host is the hub, and that is enough
//!
//! Only the host announces. Clients compare what arrives against their own log and, on a
//! mismatch, send their side of it back so the host learns too.
//!
//! That is complete rather than merely cheap. Two clients that both agree with the host agree
//! with each other, so comparing everyone against one peer catches every disagreement — at `n`
//! messages per interval instead of `n²`. It also matches the topology: `LobbyMessage` from a
//! client reaches the host and nobody else, so a mesh is not on offer here anyway.
//!
//! # Unreliable, deliberately
//!
//! Every other message in this crate is reliable because losing one breaks the simulation. This
//! one is different in both directions.
//!
//! A divergence is permanent — two peers that disagree at tick N still disagree at tick N+64 —
//! so a dropped report costs one interval of detection latency and nothing else. Against that,
//! the reliable channel is ordered, so anything put on it shares head-of-line blocking with
//! `AuthoritativeTick` and `ClientScheduledActions`, which are the two messages the tick rate
//! depends on. A diagnostic that stalls the thing it measures is worse than one that arrives a
//! second late.
//!
//! # It reports the first tick, then stops
//!
//! [`Desync`] latches. Once a mismatch is recorded this module stops comparing, for the same
//! reason `first_divergence` returns the earliest tick rather than the newest: the first
//! disagreement is the diagnosis, and everything after it is that same divergence being
//! restated once a second for as long as the session lasts.
//!
//! The latch clears when the lobby goes, so joining a different session may report afresh.
//!
//! # What it does not do
//!
//! Notice, not repair. This names the tick and the sections; it does not resynchronise anybody,
//! and nothing here decides whether a desync should end the session — that is a game's call, and
//! it makes it by reading [`DesyncDetected`].

use bevy::prelude::*;
use bevy_ensemble::{EnsembleAppExt, Host, Lobby, LobbyMessage, ReceivedEnsembleMessage, SendMode};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::marker::PhantomData;

use crate::checksum::{ChecksumLog, Divergence, WorldHash};

/// What one peer's world hashed to at one tick.
///
/// Carries the whole `H` rather than [`WorldHash::value`]'s `u64`, and that is the difference
/// between a report worth reading and one that is not. A receiver holding its own `H` and the
/// other side's *number* can say "tick 4096 differs" and no more;
/// [`differences`](WorldHash::differences) needs both values, and it is the method that turns a
/// divergence into a direction to walk in — "the players differ but the buildings do not" and the
/// reverse send you to opposite ends of a codebase.
#[derive(Message, Serialize, Deserialize, Debug, Clone, Copy)]
pub struct ChecksumReport<H> {
    pub tick: u64,
    pub hash: H,
}

/// Two peers disagreed, and this is where.
///
/// Written once per session. Read it to log, to show the player something, or to end the session
/// — this crate does none of those, because which of them is right is a property of the game.
#[derive(Message, Debug, Clone)]
pub struct DesyncDetected<H: WorldHash> {
    /// The peer whose report disagreed with ours.
    pub peer: u128,
    /// The tick, both hashes, and which sections differ.
    pub divergence: Divergence<H>,
}

/// The divergence this peer has already reported, if any.
///
/// Present means "stop comparing" — see the module note on why the first tick is the whole
/// answer. Removed when the lobby goes.
#[derive(Resource, Debug, Clone)]
pub struct Desync<H: WorldHash> {
    pub peer: u128,
    pub divergence: Divergence<H>,
}

/// Reports that arrived before this peer had simulated the tick they describe.
///
/// Not an edge case — the *normal* case, and the reason this buffer exists rather than a
/// straight compare-on-receipt. A client is behind the host by construction: it only advances a
/// tick once the host's authoritative message for it has arrived, so a report for tick `T`
/// broadcast the moment the host sampled `T` necessarily reaches a client that has not reached
/// `T` yet. Comparing on receipt would find no local sample and conclude nothing, for ever.
#[derive(Resource)]
pub struct PendingChecksumReports<H: WorldHash> {
    reports: Vec<(u128, ChecksumReport<H>)>,
}

impl<H: WorldHash> Default for PendingChecksumReports<H> {
    fn default() -> Self {
        Self {
            reports: Vec::new(),
        }
    }
}

/// The tick this peer last put on the wire, so the cadence follows the log rather than repeating
/// its interval arithmetic.
#[derive(Resource, Debug, Default)]
pub struct LastAnnouncedChecksum(pub Option<u64>);

/// Broadcasts this peer's world hash and compares what comes back. See the module docs.
///
/// Add it *alongside* [`ChecksumLogPlugin`](crate::checksum::ChecksumLogPlugin), which is what
/// fills the log this reads — this plugin deliberately does not add it, because where the sample
/// belongs in a game's tick is a decision only that game can make.
///
/// The `Serialize`/`DeserializeOwned` bounds sit here rather than on [`WorldHash`] so that
/// existing implementations keep compiling untouched: a game that wants section-level reports
/// derives them, and one that does not is unaffected.
pub struct ChecksumExchangePlugin<H: WorldHash>(PhantomData<fn() -> H>);

impl<H: WorldHash> Default for ChecksumExchangePlugin<H> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<H> Plugin for ChecksumExchangePlugin<H>
where
    H: WorldHash + Serialize + DeserializeOwned,
{
    fn build(&self, app: &mut App) {
        app.init_resource::<ChecksumLog<H>>()
            .init_resource::<PendingChecksumReports<H>>()
            .init_resource::<LastAnnouncedChecksum>()
            .add_message::<DesyncDetected<H>>()
            .register_ensemble_message_type::<ChecksumReport<H>>()
            .add_systems(
                Update,
                (
                    forget_desync_on_lobby_removed::<H>,
                    announce_checksum::<H>,
                    compare_arrived_checksums::<H>,
                )
                    .chain(),
            );
    }
}

/// Put the newest sample on the wire, if it is one the lobby has not been told about.
///
/// Keyed on the log's own newest tick rather than on `tick % interval`, so the cadence cannot
/// drift away from what was actually sampled — two peers comparing ticks that only one of them
/// recorded is the failure this whole module is trying not to have.
///
/// Compared with `!=` rather than `>`: a client that applies a join snapshot has its
/// [`CurrentTick`](bevy_ticked::prelude::CurrentTick) moved backwards, and a peer that never
/// announces again after a rejoin is a peer that has quietly stopped checking.
fn announce_checksum<H>(
    mut commands: Commands,
    log: Res<ChecksumLog<H>>,
    mut last_announced: ResMut<LastAnnouncedChecksum>,
    desync: Option<Res<Desync<H>>>,
    host_lobby: Option<Single<Entity, (With<Lobby>, With<Host>)>>,
) where
    H: WorldHash + Serialize + DeserializeOwned,
{
    if desync.is_some() {
        return;
    }
    let Some(host_lobby) = host_lobby else {
        return;
    };
    let Some((tick, hash)) = log.latest() else {
        return;
    };
    if last_announced.0 == Some(tick) {
        return;
    }
    last_announced.0 = Some(tick);

    let report = ChecksumReport { tick, hash };
    commands
        .entity(*host_lobby)
        .trigger(move |entity| LobbyMessage {
            entity,
            message: report,
            send_mode: SendMode::Unreliable,
        });
}

/// Compare every report we can, park the ones we cannot yet, and drop the ones we never will.
///
/// A report is in exactly one of four states, and telling them apart is most of the work:
///
/// * **we sampled that tick** — compare, and we are done with it either way;
/// * **we have not reached that tick** — park it, which is the ordinary case for a client;
/// * **that tick fell off the front of our log** — the session outran a report by more than the
///   log's capacity; drop it quietly, because a report we merely lost is not a disagreement;
/// * **we ran past that tick without sampling it** — the two peers disagree about
///   [`ChecksumLog::interval`], so there is nothing to compare now or ever. Say so once: silence
///   here reads exactly like a session that is being checked and passing.
///
/// Within one pass the earliest differing tick wins. Across passes it cannot: a report that turns
/// up after a later one has already latched finds the answer taken. That is left alone rather than
/// solved, because closing it means keeping the search open for ever on the chance of a report
/// arriving an entire [`ChecksumLog::interval`] late and out of order — a second, on a link whose
/// reordering is measured in milliseconds, and only ever costing the report a tick's precision.
fn compare_arrived_checksums<H>(
    mut commands: Commands,
    mut arrivals: MessageReader<ReceivedEnsembleMessage<ChecksumReport<H>>>,
    mut pending: ResMut<PendingChecksumReports<H>>,
    mut detected: MessageWriter<DesyncDetected<H>>,
    log: Res<ChecksumLog<H>>,
    desync: Option<Res<Desync<H>>>,
    client_lobby: Option<Single<Entity, (With<Lobby>, Without<Host>)>>,
) where
    H: WorldHash + Serialize + DeserializeOwned,
{
    if desync.is_some() {
        pending.reports.clear();
        arrivals.clear();
        return;
    }

    for arrival in arrivals.read() {
        let Some(sender) = arrival.sender else {
            continue;
        };
        pending.reports.push((sender, arrival.message));
    }
    if pending.reports.is_empty() {
        return;
    }

    let oldest = log.oldest().map(|(tick, _)| tick);
    let newest = log.latest().map(|(tick, _)| tick);
    let mut found: Option<(u128, Divergence<H>)> = None;

    // Ascending, so that the first mismatch this pass finds is also the earliest one, which is the
    // only one worth reporting. Arrival order will not do it: these reports travel unordered by
    // choice, and a client that has been parking them while it catches up compares a whole run of
    // ticks in one pass — so "the first we happened to look at" and "the first that differed" are
    // routinely different ticks, and the wrong one names a symptom.
    pending.reports.sort_by_key(|(_, report)| report.tick);

    pending.reports.retain(|(sender, report)| {
        if let Some(ours) = log.at(report.tick) {
            if ours != report.hash && found.is_none() {
                found = Some((
                    *sender,
                    Divergence {
                        tick: report.tick,
                        left: ours,
                        right: report.hash,
                        sections: ours.differences(&report.hash),
                    },
                ));
            }
            return false;
        }
        if oldest.is_some_and(|oldest| report.tick < oldest) {
            return false;
        }
        if newest.is_some_and(|newest| report.tick < newest) {
            warn!(
                "a peer reported a checksum for tick {} and this peer never sampled that tick, \
                 so the two cannot be compared. Both sides must agree on ChecksumLog::interval.",
                report.tick
            );
            return false;
        }
        true
    });

    let Some((peer, divergence)) = found else {
        return;
    };

    // Our side of it, so the host learns what this client saw. A client is the only peer that can
    // usefully send this: the host is already the one announcing, and a report bounced back to a
    // client would arrive at a peer that cannot act on it.
    if let Some(client_lobby) = client_lobby {
        let ours = ChecksumReport {
            tick: divergence.tick,
            hash: divergence.left,
        };
        commands
            .entity(*client_lobby)
            .trigger(move |entity| LobbyMessage {
                entity,
                message: ours,
                send_mode: SendMode::Unreliable,
            });
    }

    error!("lockstep desync against peer {peer}: {divergence}");
    detected.write(DesyncDetected {
        peer,
        divergence: divergence.clone(),
    });
    commands.insert_resource(Desync { peer, divergence });
}

/// Forget a session's divergence when its lobby goes, so the next one is judged on its own.
fn forget_desync_on_lobby_removed<H>(
    mut commands: Commands,
    mut removed_lobbies: RemovedComponents<Lobby>,
    mut pending: ResMut<PendingChecksumReports<H>>,
    mut last_announced: ResMut<LastAnnouncedChecksum>,
) where
    H: WorldHash,
{
    if removed_lobbies.read().next().is_none() {
        return;
    }
    pending.reports.clear();
    last_announced.0 = None;
    commands.remove_resource::<Desync<H>>();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
    struct TestHash {
        buildings: u64,
        players: u64,
    }

    impl WorldHash for TestHash {
        fn sample(_world: &mut World) -> Self {
            TestHash {
                buildings: 0,
                players: 0,
            }
        }

        fn value(&self) -> u64 {
            self.buildings ^ self.players
        }

        fn differences(&self, other: &Self) -> Vec<&'static str> {
            let mut differences = Vec::new();
            if self.buildings != other.buildings {
                differences.push("buildings");
            }
            if self.players != other.players {
                differences.push("players");
            }
            differences
        }
    }

    fn hash(buildings: u64, players: u64) -> TestHash {
        TestHash { buildings, players }
    }

    /// An app with the comparison system and a log, but no transport: reports are injected as
    /// though they had arrived, which is the only part of the wire this system's logic depends on.
    fn peer_with(samples: &[(u64, TestHash)]) -> App {
        let mut app = App::new();
        let mut log = ChecksumLog::<TestHash>::every_tick();
        for (tick, hash) in samples {
            log.samples.push((*tick, *hash));
        }
        app.insert_resource(log)
            .init_resource::<PendingChecksumReports<TestHash>>()
            .add_message::<DesyncDetected<TestHash>>()
            .add_message::<ReceivedEnsembleMessage<ChecksumReport<TestHash>>>()
            .add_systems(Update, compare_arrived_checksums::<TestHash>);
        app
    }

    fn deliver(app: &mut App, sender: u128, tick: u64, hash: TestHash) {
        app.world_mut()
            .resource_mut::<Messages<ReceivedEnsembleMessage<ChecksumReport<TestHash>>>>()
            .write(ReceivedEnsembleMessage {
                sender: Some(sender),
                message: ChecksumReport { tick, hash },
                received_at: core::time::Duration::ZERO,
            });
    }

    fn desync_of(app: &App) -> Option<Desync<TestHash>> {
        app.world().get_resource::<Desync<TestHash>>().cloned()
    }

    #[test]
    fn agreement_reports_nothing() {
        let mut app = peer_with(&[(64, hash(1, 2))]);
        deliver(&mut app, 7, 64, hash(1, 2));
        app.update();

        assert!(desync_of(&app).is_none(), "the peers agree about tick 64");
    }

    #[test]
    fn a_disagreement_names_the_tick_and_the_sections() {
        let mut app = peer_with(&[(64, hash(1, 2))]);
        deliver(&mut app, 7, 64, hash(99, 2));
        app.update();

        let desync = desync_of(&app).expect("the peers disagree about tick 64");
        assert_eq!(desync.peer, 7);
        assert_eq!(desync.divergence.tick, 64);
        assert_eq!(
            desync.divergence.sections,
            vec!["buildings"],
            "a report that only says the tick leaves the reader bisecting a u64"
        );
    }

    #[test]
    fn a_report_for_a_tick_we_have_not_reached_waits_rather_than_being_dropped() {
        // The ordinary case for a client, which trails the host by construction.
        let mut app = peer_with(&[(64, hash(1, 2))]);
        deliver(&mut app, 7, 128, hash(9, 9));
        app.update();

        assert!(
            desync_of(&app).is_none(),
            "tick 128 is not simulated here yet, so there is nothing to disagree with"
        );

        app.world_mut()
            .resource_mut::<ChecksumLog<TestHash>>()
            .samples
            .push((128, hash(1, 1)));
        app.update();

        assert_eq!(
            desync_of(&app).map(|desync| desync.divergence.tick),
            Some(128),
            "the parked report has to be compared once the tick lands, or a client — which is \
             always behind — never checks anything at all"
        );
    }

    #[test]
    fn the_first_differing_tick_is_the_one_reported() {
        let mut app = peer_with(&[(64, hash(1, 2)), (128, hash(3, 4))]);
        deliver(&mut app, 7, 128, hash(30, 40));
        deliver(&mut app, 7, 64, hash(10, 20));
        app.update();

        assert_eq!(
            desync_of(&app).map(|desync| desync.divergence.tick),
            Some(64),
            "reporting 128 would name a symptom of whatever went wrong at 64"
        );
    }

    #[test]
    fn a_reported_desync_latches_and_stops_the_comparison() {
        let mut app = peer_with(&[(64, hash(1, 2)), (128, hash(3, 4))]);
        deliver(&mut app, 7, 64, hash(10, 20));
        app.update();
        deliver(&mut app, 7, 128, hash(30, 40));
        app.update();

        assert_eq!(
            desync_of(&app).map(|desync| desync.divergence.tick),
            Some(64),
            "the divergence is permanent, so every later tick differs too — re-reporting it once \
             a second buries the one number that mattered"
        );
    }

    #[test]
    fn a_tick_that_aged_out_of_the_log_is_not_a_disagreement() {
        let mut app = peer_with(&[(128, hash(1, 2))]);
        deliver(&mut app, 7, 64, hash(9, 9));
        app.update();

        assert!(
            desync_of(&app).is_none(),
            "tick 64 fell off the front of the log; a sample we no longer hold is not evidence \
             that it differed"
        );
        assert!(
            app.world()
                .resource::<PendingChecksumReports<TestHash>>()
                .reports
                .is_empty(),
            "and it must not be parked for ever, or the buffer grows without bound"
        );
    }
}
