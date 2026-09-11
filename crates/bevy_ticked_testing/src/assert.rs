//! Assertions in the vocabulary of the stack: agreement, replay purity, id invariants, byte and
//! correction budgets, log hygiene.
//!
//! Each one panics with the number that failed and the tick it failed on, and each one guards
//! against its own vacuity — a replay check whose hash never changed, an agreement check with no
//! tick to compare, a bandwidth check across zero ticks. A netcode assertion that can pass
//! without measuring anything is worse than none: it is the green light on the dashboard of a
//! session that has already desynced.

use std::any::type_name;

use bevy::prelude::*;
use bevy_ensemble_loopback::PeerId;
use bevy_ticked::TickedSimulation;
use bevy_ticked::checksum::{ChecksumLog, Divergence, WorldHash};
use bevy_ticked::events::TickedEventRegistry;
use bevy_ticked::registry::{TickedComponent, TickedComponentRegistry};
use bevy_ticked::tick::CurrentTick;
use bevy_ticked::time::run_tick_schedule;
use bevy_ticked::tracked_entity::TickTrackedEntity;

use crate::log::{LogMark, warnings_since};
use crate::net::{Role, TickedNetwork};
use crate::view::{latest, replays, role, tick, tracked_ids};

// ---- agreement -------------------------------------------------------------------------------

fn checksum_log<H: WorldHash>(app: &App, peer: PeerId) -> &ChecksumLog<H> {
    app.world()
        .get_resource::<ChecksumLog<H>>()
        .unwrap_or_else(|| {
            panic!(
                "peer {peer:?} has no ChecksumLog<{}>: add ChecksumLogPlugin to every peer",
                type_name::<H>()
            )
        })
}

/// The ticks at or after `since` that both `a` and `b` sampled — the only ones an agreement
/// check can say anything about.
pub fn compared_ticks_since<H: WorldHash>(
    net: &TickedNetwork,
    a: PeerId,
    b: PeerId,
    since: u64,
) -> Vec<u64> {
    let left = checksum_log::<H>(net.app(a), a);
    let right = checksum_log::<H>(net.app(b), b);
    let mut ticks: Vec<u64> = left
        .samples
        .iter()
        .map(|(tick, _)| *tick)
        .filter(|tick| *tick >= since && right.at(*tick).is_some())
        .collect();
    ticks.sort_unstable();
    ticks.dedup();
    ticks
}

/// Every tick both `a` and `b` sampled.
pub fn compared_ticks<H: WorldHash>(net: &TickedNetwork, a: PeerId, b: PeerId) -> Vec<u64> {
    compared_ticks_since::<H>(net, a, b, 0)
}

/// The earliest tick at or after `since` that `a` and `b` both sampled and disagree on.
///
/// Over each peer's `ChecksumLog<H>`, with the log's own reading of it: the *first* sample a
/// peer recorded for a tick. In lockstep there is one per tick and it is the desync. On a
/// predicting client the log also holds every replay of a tick, and the first sample is the
/// prediction — so this reports the first *misprediction*, corrected or not, which is the right
/// question for a client that has stopped hearing its host and the wrong one for a client under
/// correction. For that, [`assert_converged`].
pub fn first_divergence_since<H: WorldHash>(
    net: &TickedNetwork,
    a: PeerId,
    b: PeerId,
    since: u64,
) -> Option<Divergence<H>> {
    let left = checksum_log::<H>(net.app(a), a);
    let right = checksum_log::<H>(net.app(b), b);
    left.samples
        .iter()
        .filter(|(tick, _)| *tick >= since)
        .filter_map(|(tick, mine)| {
            right
                .at(*tick)
                .filter(|theirs| theirs != mine)
                .map(|theirs| Divergence {
                    tick: *tick,
                    left: *mine,
                    right: theirs,
                    sections: mine.differences(&theirs),
                })
        })
        .min_by_key(|divergence| divergence.tick)
}

/// The earliest tick `a` and `b` both sampled and disagree on.
pub fn first_divergence<H: WorldHash>(
    net: &TickedNetwork,
    a: PeerId,
    b: PeerId,
) -> Option<Divergence<H>> {
    first_divergence_since::<H>(net, a, b, 0)
}

/// Every client's log agrees with the host's on every tick at or after `since` that both
/// sampled.
///
/// Every client, whatever its attachment: a disconnected or half-open client is still running,
/// still sampling, and is exactly the replica most likely to have drifted. A check that skipped
/// it would pass on the session it was written to catch.
///
/// `since` is for a state-sync session, where the ticks before a client learnt of a host-side
/// spawn are mispredictions by construction. Take it as the client's tick a couple of frames
/// after the spawn.
///
/// # Panics
///
/// Naming the tick, both hashes and the differing sections of the first disagreement — or, if
/// there is no client, or some client sampled no tick in common with the host, saying so: an
/// agreement check with nothing to compare has checked nothing.
pub fn assert_all_peers_agree_since<H: WorldHash>(net: &TickedNetwork, since: u64) {
    let host = net.host();
    let clients = net.clients();
    assert!(
        !clients.is_empty(),
        "there is no client on this network for the host to agree with"
    );
    for client in clients {
        let compared = compared_ticks_since::<H>(net, host, client, since);
        assert!(
            !compared.is_empty(),
            "the host and client {client:?} (uuid {}) sampled no tick in common at or after tick \
             {since}: nothing was compared. Check the logs' intervals, that both peers have run \
             past {since}, and that the log's capacity covers the window",
            net.uuid(client)
        );
        if let Some(divergence) = first_divergence_since::<H>(net, host, client, since) {
            panic!(
                "the host (uuid {}) and client {client:?} (uuid {}) disagree, first at {divergence}. \
                 {} ticks compared, from {} to {}",
                net.uuid(host),
                net.uuid(client),
                compared.len(),
                compared[0],
                compared[compared.len() - 1],
            );
        }
    }
}

/// Every attached client's log agrees with the host's on every tick both sampled.
pub fn assert_all_peers_agree<H: WorldHash>(net: &TickedNetwork) {
    assert_all_peers_agree_since::<H>(net, 0);
}

// ---- replay purity ---------------------------------------------------------------------------

/// Simulating `ticks` ticks from `from`, rolling back to `from` and simulating them again
/// produces the same `H` at every tick.
///
/// The property rollback depends on and nothing else checks: that every piece of state the
/// simulation reads is either registered — and so restored — or derived from something that is.
/// A physics body whose velocity is not registered replays from its *post*-roll velocity; a
/// system with a `Local`; a component that is written but never read back until the next tick.
/// Each of those is a client that corrects itself into a different world than the host's, and
/// each shows up here as the tick the replay first differed on.
///
/// Uses the default poison: after the restore and before the replay, every tracked entity's
/// `Transform` is moved to `(999, 999, 999)`. A registered transform is restored over it; an
/// unregistered one that the simulation reads is now visibly wrong. See
/// [`assert_replays_identically_with`] to poison something else.
pub fn assert_replays_identically<H: WorldHash>(app: &mut App, from: u64, ticks: u64) {
    assert_replays_identically_with::<H>(app, from, ticks, poison_transforms);
}

fn poison_transforms(world: &mut World) {
    let mut tracked = world.query_filtered::<&mut Transform, With<TickTrackedEntity>>();
    for mut transform in tracked.iter_mut(world) {
        transform.translation = Vec3::splat(999.0);
    }
}

/// [`assert_replays_identically`] with `poison` applied to the world after the restore and
/// before the replay, in place of the default.
///
/// `app` is stepped one frame per tick to reach `from` and to record the live run, so it needs
/// the `Hz` source fed one [`TICK`](crate::peer::TICK) per frame — what
/// [`peer_app_with`](crate::peer::peer_app_with) builds — and must not be a client, whose
/// `PreTick` would apply snapshots between the ticks being recorded.
///
/// # Panics
///
/// On the first tick whose replayed hash differs from the live one, naming it and the differing
/// sections; if the live run never changed the hash, since agreement about nothing is no
/// evidence; or if a frame turned into anything but one tick.
pub fn assert_replays_identically_with<H: WorldHash>(
    app: &mut App,
    from: u64,
    ticks: u64,
    poison: impl Fn(&mut World),
) {
    assert!(
        ticks >= 2,
        "a replay of fewer than two ticks cannot show a change"
    );
    assert_ne!(
        role(app),
        Role::Client,
        "run the purity check on a host or solo peer: a client's PreTick applies snapshots \
         between the ticks being recorded"
    );
    assert!(
        tick(app) <= from,
        "this peer is already at tick {} and the check starts at {from}: start it at or after \
         the current tick",
        tick(app)
    );

    // Reach `from`.
    let mut frames = 0u64;
    while tick(app) < from {
        app.update();
        frames += 1;
        assert!(
            frames <= from + 64,
            "after {frames} frames this peer is at tick {}, not {from}: is it paused, or on a \
             source other than Hz fed one TICK per frame?",
            tick(app)
        );
    }

    // The live run, one tick per frame, sampled after each.
    let mut live: Vec<(u64, H)> = Vec::with_capacity(ticks as usize);
    for expected in (from + 1)..=(from + ticks) {
        app.update();
        let now = tick(app);
        assert_eq!(
            now, expected,
            "one frame was not one tick: at tick {now}, expected {expected}. The purity check \
             needs the Hz source fed exactly one TICK per frame and no rate dilation"
        );
        live.push((expected, H::sample(app.world_mut())));
    }
    assert!(
        live.windows(2).any(|pair| pair[0].1 != pair[1].1),
        "the hash never changed over ticks {}..={}: nothing was simulated, so a replay that \
         agrees proves nothing",
        from + 1,
        from + ticks
    );

    // Roll back, poison, replay — the client's rollback path, with a sample after each tick.
    let world = app.world_mut();
    let registry = world.resource::<TickedComponentRegistry>().clone();
    registry.restore_all(world, from);
    world.resource_mut::<CurrentTick>().0 = from;
    registry.truncate_all_after(world, from);
    TickedEventRegistry::truncate_all_after(world, from);
    poison(world);

    for (replayed, expected) in live {
        world.resource_mut::<CurrentTick>().0 = replayed;
        run_tick_schedule(world, replayed, TickedSimulation);
        registry.capture_all(world, replayed);
        let got = H::sample(world);
        if got != expected {
            let sections = expected.differences(&got);
            panic!(
                "the replay diverged at tick {replayed} ({} ticks into a replay from {from}): \
                 live {:#018x}, replayed {:#018x}, differing in: {}. Something the simulation \
                 reads at that tick is not restored by rollback",
                replayed - from,
                expected.value(),
                got.value(),
                if sections.is_empty() {
                    "nothing identifiable".to_string()
                } else {
                    sections.join(", ")
                }
            );
        }
    }
}

// ---- ids -------------------------------------------------------------------------------------

/// Every tracked id on every client is one the host has held since
/// [`record_issued_ids`](TickedNetwork::record_issued_ids) was switched on.
///
/// The invariant behind `apply_snapshot` keying the world by id: an id a client made up
/// collides with the next one the host hands out, and the two entities are merged into
/// whichever the client happens to hold. It is not hypothetical — the join window, between a
/// lobby forming and the role being adopted, is where a solo spawner keeps spawning.
///
/// # Panics
///
/// Listing the ids per client, or if recording was never switched on.
pub fn assert_no_id_unissued(net: &mut TickedNetwork) {
    let issued = net
        .issued_ids()
        .cloned()
        .expect("call `record_issued_ids()` on the network before asserting about issued ids");
    for client in net.clients() {
        let uuid = net.uuid(client);
        let unissued: Vec<u64> = tracked_ids(net.app_mut(client))
            .into_iter()
            .filter(|id| !issued.contains(id))
            .collect();
        assert!(
            unissued.is_empty(),
            "client {client:?} (uuid {uuid}) holds tracked ids the host never issued: \
             {unissued:?}. Something on the client minted them, and they will collide with the \
             host's next spawn"
        );
    }
}

/// No tracked id is carried by two entities in this world.
pub fn assert_ids_unique(app: &mut App) {
    let ids = tracked_ids(app);
    let duplicates: Vec<u64> = ids
        .windows(2)
        .filter(|pair| pair[0] == pair[1])
        .map(|pair| pair[0])
        .collect();
    assert!(
        duplicates.is_empty(),
        "a tracked id is carried by more than one entity: {duplicates:?} (all ids: {ids:?})"
    );
}

// ---- budgets ---------------------------------------------------------------------------------

/// Over `frames` frames, `from` sent `to` no more than `max_bytes_per_tick` bytes per tick the
/// sender simulated. Returns the measured bytes per tick, so a test can print the figure it
/// budgets against.
///
/// Per tick rather than per frame, so the budget survives a peer that runs two ticks in a frame.
/// Everything on the wire counts — snapshots, pings, roster — because that is what the link
/// carries.
///
/// # Panics
///
/// With the measured figure, or if the sender ran no tick in the window.
pub fn assert_bandwidth_within(
    net: &mut TickedNetwork,
    from: PeerId,
    to: PeerId,
    frames: usize,
    max_bytes_per_tick: usize,
) -> f64 {
    let bytes_before = net.bytes_sent(from, to);
    let tick_before = tick(net.app(from));
    net.run(frames);
    let bytes = net.bytes_sent(from, to) - bytes_before;
    let ticks = tick(net.app(from)) - tick_before;
    assert!(
        ticks > 0,
        "peer {from:?} ran no tick in {frames} frames, so there is no per-tick figure to budget"
    );
    let per_tick = bytes as f64 / ticks as f64;
    assert!(
        per_tick <= max_bytes_per_tick as f64,
        "{from:?} -> {to:?} sent {bytes} bytes over {ticks} ticks: {per_tick:.1} bytes per tick, \
         budget {max_bytes_per_tick}"
    );
    per_tick
}

/// This client's rollback counters are within budget: no more than `max_rollbacks` corrections
/// and `max_ticks_replayed` ticks re-simulated, since the counters were last reset.
pub fn assert_replays_within(app: &App, max_rollbacks: u64, max_ticks_replayed: u64) {
    let stats = replays(app);
    assert!(
        stats.rollbacks <= max_rollbacks,
        "{} rollbacks, budget {max_rollbacks} ({stats:?})",
        stats.rollbacks
    );
    assert!(
        stats.ticks_replayed <= max_ticks_replayed,
        "{} ticks replayed, budget {max_ticks_replayed} ({stats:?})",
        stats.ticks_replayed
    );
}

/// Every client's current `T` on tracked id `id` is within `tolerance` of the host's, by
/// `distance`.
///
/// The right agreement check for a state-sync session, where peers are expected to have been
/// *told* the same thing rather than to have computed it bit for bit.
///
/// # Panics
///
/// If the host has no `T` on `id` — the check would be vacuous — or a client has none, or one
/// is further than `tolerance` away.
pub fn assert_converged<T: TickedComponent>(
    net: &TickedNetwork,
    id: u64,
    distance: impl Fn(&T, &T) -> f32,
    tolerance: f32,
) {
    let host = net.host();
    let truth = latest::<T>(net.app(host), id).unwrap_or_else(|| {
        panic!(
            "the host has no `{}` on tracked id {id}: nothing to converge to",
            type_name::<T>()
        )
    });
    for client in net.clients() {
        let uuid = net.uuid(client);
        let replica = latest::<T>(net.app(client), id).unwrap_or_else(|| {
            panic!(
                "client {client:?} (uuid {uuid}) has no `{}` on tracked id {id}: the host's was \
                 never replicated. Is the component registered on both peers, in the same order?",
                type_name::<T>()
            )
        });
        let far = distance(&truth, &replica);
        assert!(
            far <= tolerance,
            "client {client:?} (uuid {uuid}) is {far} from the host on `{}` for tracked id {id}, \
             tolerance {tolerance}",
            type_name::<T>()
        );
    }
}

// ---- logs ------------------------------------------------------------------------------------

/// Nothing has been logged at `warn!` or above since `mark`. Per test thread; see
/// [`log`](crate::log) for exactly what that covers.
pub fn assert_no_warnings(mark: &LogMark) {
    assert_no_warnings_matching(mark, &[]);
}

/// Nothing has been logged at `warn!` or above since `mark`, apart from lines containing one
/// of `allow`.
pub fn assert_no_warnings_matching(mark: &LogMark, allow: &[&str]) {
    let mut unexpected: Vec<String> = warnings_since(mark);
    unexpected.extend(crate::log::errors_since(mark));
    unexpected.retain(|line| !allow.iter().any(|allowed| line.contains(allowed)));
    assert!(
        unexpected.is_empty(),
        "{} unexpected warning(s) since the mark:\n  {}",
        unexpected.len(),
        unexpected.join("\n  ")
    );
}
