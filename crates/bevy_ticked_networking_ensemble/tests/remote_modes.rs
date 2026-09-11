//! What a client shows of a body it does not drive, over the harness.
//!
//! The audit's finding: a client simulated every tracked entity through its replay with
//! whatever input it had, which for a remote player was nothing, so a body that was walking on
//! the host stood still for the whole prediction lead on every client and then snapped to
//! where the next snapshot said it was — sixty-four times a second. Every game on the stack
//! wrote a smoothing layer over that. This suite is the stack's own answer: a remote body is
//! interpolated between authoritative states by default, a body a game chooses to predict
//! holds its last known input through the replay, the host relays what it holds so that
//! replay has something to hold, and a correction that does land is slid over rather than
//! shown.
//!
//! Host and two clients, A and B, on the integer fixture: A walks, B watches. What B shows of
//! A's body is the thing under test.

use bevy::prelude::*;
use bevy_ticked::prelude::*;
use bevy_ticked_networking::client::ClientSet;
use bevy_ticked_networking::input::InputQueue;
use bevy_ticked_networking::replication::{InterpolationDelay, ReplicationMode};
use bevy_ticked_networking::server::InputMargins;
use bevy_ticked_networking::smoothing::{
    CorrectionSmoothing, CorrectionStats, SmoothingOffset, TickedSmoothingPlugin,
};
use bevy_ticked_networking::snapshot::SnapshotBody;
use bevy_ticked_testing::fixtures::minimal::{self, Input, Pos, seat_everyone};
use bevy_ticked_testing::prelude::*;

const SETTLE: usize = 400;

/// A host and two clients, settled and seated, thirty frames into the session so every peer
/// holds every body.
struct Session {
    net: TickedNetwork,
    host: PeerId,
    /// The client that walks.
    a: PeerId,
    /// The client that watches.
    b: PeerId,
    a_uuid: u128,
    /// A's body, by tracked id.
    a_body: u64,
    /// B's own body.
    b_body: u64,
}

fn session(link: Link, build: impl Fn(&mut App) + 'static) -> Session {
    let mut net = TickedNetwork::client_server::<Input>(2, build)
        .with_link(link)
        .with_seed(21);
    assert!(
        net.settle(SETTLE),
        "the session did not settle in {SETTLE} frames over {link:?}"
    );
    let seats = seat_everyone(&mut net);
    let host = net.host();
    let clients = net.clients();
    let (a, b) = (clients[0], clients[1]);
    let (a_uuid, b_uuid) = (net.uuid(a), net.uuid(b));
    let a_body = body_of(&seats, a_uuid);
    let b_body = body_of(&seats, b_uuid);
    net.run(30);
    for peer in [a, b] {
        assert!(
            latest::<Pos>(net.app(peer), a_body).is_some(),
            "thirty frames after seating, {peer:?} does not hold A's body"
        );
    }
    Session {
        net,
        host,
        a,
        b,
        a_uuid,
        a_body,
        b_body,
    }
}

fn body_of(seats: &[(u128, u64)], uuid: u128) -> u64 {
    seats
        .iter()
        .find(|(owner, _)| *owner == uuid)
        .map(|(_, id)| *id)
        .expect("every peer was seated")
}

fn pos(app: &App, id: u64) -> i64 {
    latest::<Pos>(app, id)
        .unwrap_or_else(|| panic!("no body with tracked id {id} on this peer"))
        .0
}

fn delay_on(app: &App) -> u64 {
    app.world().resource::<InterpolationDelay>().0
}

/// The transform as the simulation last left it on `id`, through `TickedInterpolation`: what
/// the renderer's blend is heading for, before any smoothing offset.
fn shown_x(app: &App, id: u64) -> f32 {
    latest::<TickedInterpolation>(app, id)
        .and_then(|interpolation| interpolation.current())
        .map(|transform| transform.translation.x)
        .unwrap_or_else(|| panic!("tracked id {id} has no interpolated transform yet"))
}

fn mark_predicted(net: &mut TickedNetwork, peer: PeerId, id: u64) {
    let entity = tracked_entity(net.app(peer), id).expect("the body is on this peer");
    net.world_mut(peer)
        .entity_mut(entity)
        .insert(ReplicationMode::Predicted);
}

/// One frame in which `peer` presses `input`.
fn press(net: &mut TickedNetwork, peer: PeerId, input: Input) {
    let uuid = net.uuid(peer);
    queue_input(net.app_mut(peer), uuid, input);
    net.step();
}

// ── Interpolated by default ──────────────────────────────────────────────────

/// While A walks right, B's copy of A's body never goes backwards: it used to stand still for
/// the lead and then jump forward at every snapshot, and a jump forward is preceded by frames
/// of standing still, which is what this catches when it reads as "frame to frame".
#[test]
fn a_remote_body_is_never_simulated_with_zero_input() {
    let Session {
        mut net,
        host,
        a,
        b,
        a_body,
        ..
    } = session(Link::cable(), minimal::install);

    let mut last = pos(net.app(b), a_body);
    let mut frames_moved = 0;
    for frame in 0..64 {
        press(&mut net, a, Input::RIGHT);
        let now = pos(net.app(b), a_body);
        assert!(
            now >= last,
            "frame {frame}: B's copy of A's body went from {last} to {now}; it was simulated \
             with no input and put back by the snapshot"
        );
        if now > last {
            frames_moved += 1;
        }
        last = now;
    }
    assert!(
        frames_moved >= 48,
        "A walked for 64 frames and B saw its body move on {frames_moved} of them"
    );

    let truth = pos(net.app(host), a_body);
    let behind = truth - last;
    let delay = delay_on(net.app(b)) as i64;
    assert!(
        (0..=delay + 4).contains(&behind),
        "B shows A's body at {last}, the host has it at {truth}: {behind} behind, expected \
         about the interpolation delay ({delay}) plus the link"
    );
    println!("B shows A's body {behind} units behind the host (delay {delay})");
}

/// What every interpolated entity was set to after each snapshot's replay, by the tick it was
/// set from: `(display tick, tracked id, Pos)`.
#[derive(Resource, Default)]
struct Restored(Vec<(u64, u64, i64)>);

/// Reads the interpolated entities right after the restore, still inside `PreTick`, before the
/// tick moves them by one step of simulation.
fn record_restored(
    applied: Res<bevy_ticked_networking::client::AppliedSnapshotTick>,
    delay: Res<InterpolationDelay>,
    bodies: Query<(&TickTrackedEntity, &Pos, Option<&ReplicationMode>)>,
    mut restored: ResMut<Restored>,
) {
    let Some(latest) = applied.0 else { return };
    let display = latest.saturating_sub(delay.0);
    for (tracked, pos, mode) in &bodies {
        if !matches!(mode, Some(ReplicationMode::Predicted)) {
            restored.0.push((display, tracked.0, pos.0));
        }
    }
}

/// On B, A's body is exactly what the host had at `authoritative tick - delay`, never what B's
/// replay would have made of it: the replay's output is overwritten before the renderer sees
/// it, and the one tick of simulation that runs after the restore is one tick, not a lead.
#[test]
fn an_interpolated_entity_is_never_replayed() {
    let mut net = TickedNetwork::client_server::<Input>(1, minimal::install)
        .with_link(Link::cable())
        .with_seed(22);
    let b = net.add_client_with(|app| {
        app.init_resource::<Restored>().add_systems(
            TickedLoop,
            record_restored
                .in_set(TickedSystems::PreTick)
                .after(ClientSet::AfterSnapshot),
        );
    });
    assert!(net.settle(SETTLE));
    let seats = seat_everyone(&mut net);
    let (host, a) = (net.host(), net.client());
    let a_body = body_of(&seats, net.uuid(a));
    net.run(30);
    assert_eq!(
        replication_mode(net.app(b), a_body),
        None,
        "A's body carries no marker on B: interpolated by default"
    );
    net.world_mut(b).resource_mut::<Restored>().0.clear();

    for _ in 0..64 {
        press(&mut net, a, Input::RIGHT);
        let display = authoritative_tick(net.app(b)).expect("B has applied a snapshot")
            - delay_on(net.app(b));
        let truth = component_at::<Pos>(net.app(host), a_body, display)
            .expect("the host still holds history for the display tick")
            .0;
        let shown = pos(net.app(b), a_body);
        assert!(
            (shown - truth).abs() <= 1,
            "after a frame B shows A's body at {shown}; the host had it at {truth} for the \
             display tick {display}. More than one tick of simulation ran on it since the \
             restore, which is a replay"
        );
    }

    let restored = std::mem::take(&mut net.world_mut(b).resource_mut::<Restored>().0);
    let of_a: Vec<&(u64, u64, i64)> = restored.iter().filter(|(_, id, _)| *id == a_body).collect();
    assert!(of_a.len() >= 48, "the probe saw A's body restored {} times in 64 frames", of_a.len());
    for (display, id, shown) in of_a {
        // The newest record at or before, as the plugin does; on a cable every tick has one.
        let truth = (0..=*display)
            .rev()
            .take(4)
            .find_map(|tick| component_at::<Pos>(net.app(host), *id, tick))
            .expect("the host holds history around the display tick")
            .0;
        assert_eq!(
            *shown, truth,
            "restored for display tick {display}, B had A's body at {shown}; the host had {truth}"
        );
    }
}

/// Under the tick interpolation, B draws A's body from a transform that moves one unit a tick
/// at most — A's speed — and sits about the interpolation delay behind the host. The blend is
/// between two consecutive authoritative states, never a jump over the lead.
#[test]
fn an_interpolated_entity_is_drawn_between_the_two_latest_authoritative_states_with_a_delay() {
    let Session {
        mut net,
        host,
        a,
        b,
        a_body,
        ..
    } = session(Link::cable(), minimal::install_with_transform);

    let mut last = shown_x(net.app(b), a_body);
    let mut largest_step = 0.0f32;
    for frame in 0..64 {
        press(&mut net, a, Input::RIGHT);
        let now = shown_x(net.app(b), a_body);
        let step = now - last;
        assert!(
            (-1e-3..=1.0 + 1e-3).contains(&step),
            "frame {frame}: the drawn transform of A's body moved {step} in one frame; A walks \
             one unit a tick"
        );
        largest_step = largest_step.max(step);
        last = now;
    }
    assert!(largest_step > 0.5, "the drawn transform never moved");

    let truth = pos(net.app(host), a_body) as f32;
    let delay = delay_on(net.app(b)) as f32;
    let lag = truth - last;
    assert!(
        (delay - 1.0..=delay + 3.0).contains(&lag),
        "B draws A's body {lag} units behind the host; the interpolation delay is {delay} ticks"
    );
    println!("drawn {lag} units behind the host, delay {delay}");
}

// ── Predicted, with the last input held ──────────────────────────────────────

/// A game may choose to predict a remote body. On B, A's body marked `Predicted` walks through
/// every replay with A's last relayed input held, so it moves one unit per tick of B's clock
/// and never stalls — the stall was the old behaviour, a body with no input for the ticks past
/// what the host had relayed.
#[test]
fn a_predicted_remote_body_holds_its_last_input_during_replay() {
    let Session {
        mut net,
        host,
        a,
        b,
        a_body,
        ..
    } = session(Link::cable(), minimal::install);
    mark_predicted(&mut net, b, a_body);
    assert_eq!(
        replication_mode(net.app(b), a_body),
        Some(ReplicationMode::Predicted)
    );

    // Long enough for the relay to be carrying A's walk and for B's copy to have caught up.
    for _ in 0..40 {
        press(&mut net, a, Input::RIGHT);
    }
    let rollbacks_before = replays(net.app(b)).rollbacks;

    let mut last = (tick(net.app(b)), pos(net.app(b), a_body));
    for frame in 0..16 {
        press(&mut net, a, Input::RIGHT);
        let now = (tick(net.app(b)), pos(net.app(b), a_body));
        let ticks = now.0 - last.0;
        let moved = now.1 - last.1;
        assert_eq!(
            moved, ticks as i64,
            "frame {frame}: B ran {ticks} tick(s) and its predicted copy of A's body moved \
             {moved}; with A's last input held it moves once per tick, replay or not"
        );
        last = now;
    }
    assert!(
        replays(net.app(b)).rollbacks > rollbacks_before,
        "no replay ran in the window, so nothing above was tested against one"
    );
    assert!(
        pos(net.app(b), a_body) >= pos(net.app(host), a_body),
        "a predicted copy runs ahead of the host by the lead; B's is at {} and the host's at {}",
        pos(net.app(b), a_body),
        pos(net.app(host), a_body)
    );
}

// ── The relay ────────────────────────────────────────────────────────────────

/// The snapshot to B carries A's inputs for ticks after the snapshot's own: what the host
/// already holds and B's replay is about to need.
#[test]
fn the_server_relays_inputs_it_holds_for_ticks_after_the_snapshot_tick() {
    let Session {
        mut net,
        host,
        a,
        b,
        a_uuid,
        ..
    } = session(Link::cable(), minimal::install);
    net.trace_packets();
    for _ in 0..32 {
        press(&mut net, a, Input::RIGHT);
    }

    let packets = net.decode_snapshots(host, b);
    assert!(packets.len() >= 16, "few snapshots were traced: {}", packets.len());
    let mut ahead = 0usize;
    let mut with_a = 0usize;
    for packet in &packets {
        let SnapshotBody::Full(body) = &packet.body else {
            continue;
        };
        for relayed in &body.inputs_ahead {
            if relayed.player != a_uuid {
                continue;
            }
            with_a += 1;
            assert!(
                relayed.tick > packet.tick,
                "the snapshot for tick {} relays A's input for tick {}, which is not ahead of it",
                packet.tick,
                relayed.tick
            );
            ahead += 1;
        }
    }
    assert!(
        ahead > 0,
        "no snapshot to B carried an input of A's for a tick after its own ({with_a} relayed)"
    );
    println!("{ahead} of A's inputs relayed ahead of the snapshot tick over {} packets", packets.len());
}

/// B's queue holds A's inputs up to the snapshot tick plus A's margin: the first stretch of
/// every replay is simulated from what A actually pressed, and hold-last only carries the rest.
#[test]
fn relayed_inputs_cover_the_first_margin_of_the_replay() {
    let Session {
        mut net,
        host,
        a,
        b,
        a_uuid,
        ..
    } = session(Link::cable(), minimal::install);
    for _ in 0..64 {
        press(&mut net, a, Input::RIGHT);
    }

    let applied = applied_tick(net.app(b)).expect("B has applied a snapshot");
    let newest = input_queue::<Input>(net.app(b))
        .newest_for(a_uuid)
        .expect("B holds an input of A's at all");
    let margin = net
        .app(host)
        .world()
        .resource::<InputMargins>()
        .0
        .get(&a_uuid)
        .copied()
        .expect("the host has heard from A");
    assert!(margin > 0, "A's inputs are arriving late at the host (margin {margin})");
    // The margin is the host's newest measurement; the relayed set travelled a frame or two
    // earlier, so allow that much.
    let floor = applied as i64 + margin - 2;
    assert!(
        newest as i64 >= floor,
        "B holds A's inputs up to tick {newest}; the snapshot tick is {applied} and A's margin \
         {margin}, so at least tick {floor} should have been relayed"
    );
    println!("B holds A's inputs to tick {newest}: snapshot {applied} + margin {margin}");
}

// ── Correction smoothing ─────────────────────────────────────────────────────

fn attach_smoothing(add: On<Add, TickTrackedEntity>, mut commands: Commands) {
    commands
        .entity(add.entity)
        .insert(CorrectionSmoothing::default());
}

fn install_with_smoothing(app: &mut App) {
    minimal::install_with_transform(app);
    app.add_plugins(TickedSmoothingPlugin)
        .add_observer(attach_smoothing);
}

fn offset_on(app: &App, id: u64) -> Option<Vec3> {
    latest::<SmoothingOffset>(app, id).map(|offset| offset.translation)
}

fn stats_on(app: &App) -> CorrectionStats {
    *app.world().resource::<CorrectionStats>()
}

/// Make `id` wrong by one unit on `peer` in a way the next snapshot corrects *visibly*: the
/// snapshots are held off long enough for the wrong `Pos` to reach the transform, so that when
/// one lands, the transform moves. A corruption alone is put right before the renderer sees it
/// — and so is one made while a snapshot is still on the link: the drop takes effect at the
/// send, so the link is given two frames to drain before the body is touched.
fn corrupt_visibly(net: &mut TickedNetwork, host: PeerId, peer: PeerId, id: u64) {
    drop_next_packets(net, host, peer, 6);
    net.run(2);
    corrupt_component::<Pos>(net.app_mut(peer), id, |pos| pos.0 += 1);
    net.run(2);
    assert_eq!(
        latest::<Transform>(net.app(peer), id).map(|t| t.translation.x),
        Some(pos(net.app(peer), id) as f32),
        "the wrong Pos never reached the transform, so no correction can be seen"
    );
}

/// A correction to the local player's own body is felt, never smoothed: smoothing it makes
/// input feel late. The exemption is by `Owner`, not by a marker the game has to remember.
#[test]
fn correction_smoothing_never_touches_the_local_player() {
    let Session {
        mut net,
        host,
        b,
        a_body,
        b_body,
        ..
    } = session(Link::cable(), install_with_smoothing);
    net.world_mut(b).resource_mut::<CorrectionStats>().reset();

    // The local player's body goes wrong and is corrected.
    corrupt_visibly(&mut net, host, b, b_body);
    for frame in 0..16 {
        net.step();
        assert_eq!(
            offset_on(net.app(b), b_body),
            None,
            "frame {frame}: a smoothing offset appeared on the local player's own body"
        );
    }
    assert_eq!(
        pos(net.app(b), b_body),
        pos(net.app(host), b_body),
        "the corruption was never corrected, so the exemption above was not exercised"
    );
    let stats = stats_on(net.app(b));
    assert_eq!(
        stats.corrections, 0,
        "a correction of the local player's body was counted: {stats:?}"
    );

    // A predicted remote body goes wrong the same way and is smoothed.
    mark_predicted(&mut net, b, a_body);
    corrupt_visibly(&mut net, host, b, a_body);
    let landed = net.run_until(16, |net| offset_on(net.app(b), a_body).is_some());
    assert!(landed, "the corrected remote body never got a smoothing offset");
    let stats = stats_on(net.app(b));
    assert!(stats.corrections >= 1, "the correction was not counted: {stats:?}");
    assert_eq!(stats.snapped, 0, "a one-unit correction was shown as a jump: {stats:?}");
    assert_eq!(
        offset_on(net.app(b), b_body),
        None,
        "the local player's body picked up an offset from the remote body's correction"
    );
}

/// The offset that hides a small correction decays every frame and only ever shrinks: the eye
/// sees a slide, not a blink, and not a slide that stops halfway and jumps the rest.
#[test]
fn a_small_correction_decays_and_never_snaps() {
    let Session {
        mut net,
        host,
        b,
        a_body,
        ..
    } = session(Link::cable(), install_with_smoothing);
    mark_predicted(&mut net, b, a_body);
    net.world_mut(b).resource_mut::<CorrectionStats>().reset();
    corrupt_visibly(&mut net, host, b, a_body);
    assert!(
        net.run_until(16, |net| offset_on(net.app(b), a_body).is_some()),
        "no smoothing offset appeared"
    );

    let smoothing = latest::<CorrectionSmoothing>(net.app(b), a_body).expect("smoothed");
    let per_frame = (-smoothing.decay_rate * TICK.as_secs_f32()).exp();
    let mut last = offset_on(net.app(b), a_body).unwrap().length();
    assert!(last > 0.5, "the offset is {last}, smaller than the one-unit correction");
    let mut frames = 0;
    let mut below_a_hundredth = None;
    while last >= 1e-3 {
        net.step();
        frames += 1;
        assert!(frames <= 64, "the offset is still {last} a second after the correction");
        let now = offset_on(net.app(b), a_body)
            .map(|offset| offset.length())
            .unwrap_or(0.0);
        assert!(
            now < last,
            "frame {frames}: the offset went from {last} to {now}; it must only ever shrink"
        );
        if below_a_hundredth.is_none() && now < 1e-2 {
            below_a_hundredth = Some(frames);
        }
        last = now;
    }
    // At `decay_rate` 12 the offset keeps 83% of itself per frame: under a hundredth takes
    // about twenty-five frames. Much fewer is a snap dressed as a decay.
    let expected = (1e-2f32).ln() / per_frame.ln();
    let took = below_a_hundredth.expect("the offset fell below a hundredth") as f32;
    assert!(
        took >= expected * 0.6,
        "the offset fell below a hundredth in {took} frames; at this decay rate that takes \
         about {expected:.0}"
    );
    let stats = stats_on(net.app(b));
    assert_eq!(stats.snapped, 0, "{stats:?}");
    println!("decayed below 1e-3 in {frames} frames (below 1e-2 in {took})");
}

/// A remote body drawn from the authoritative history moves by what it moved by, per tick.
///
/// Measured per *tick the viewer ran*: a client frame that ran two ticks (its lead being
/// trimmed) shows two ticks of motion, which is not a snap. The audit saw the body jump by
/// the whole lead at every snapshot. The display clock advances one tick per tick, two when
/// a bunch of late snapshots has left it behind, so a walking body at one unit per tick is
/// drawn moving at most two units per tick, and mostly one.
#[test]
fn remote_bodies_no_longer_snap_at_every_snapshot() {
    let Session {
        mut net,
        a,
        b,
        a_body,
        ..
    } = session(Link::bad_wifi(), minimal::install_with_transform);

    let mut last = shown_x(net.app(b), a_body);
    let mut last_tick = tick(net.app(b));
    let mut largest_per_tick = 0.0f32;
    let mut at_frame = 0;
    let mut steps = std::collections::BTreeMap::<i64, usize>::new();
    for frame in 0..100 {
        press(&mut net, a, Input::RIGHT);
        let now = shown_x(net.app(b), a_body);
        let ticks = (tick(net.app(b)) - last_tick).max(1) as f32;
        let per_tick = (now - last).abs() / ticks;
        *steps.entry(per_tick.round() as i64).or_default() += 1;
        if per_tick > largest_per_tick {
            largest_per_tick = per_tick;
            at_frame = frame;
        }
        last = now;
        last_tick = tick(net.app(b));
    }
    let lead = lead(net.app(b), net.app(net.host()));
    println!(
        "per-tick moves of A's body as drawn on B: {steps:?}; largest {largest_per_tick} at \
         frame {at_frame}; B leads by {lead}"
    );
    assert!(
        largest_per_tick <= 2.0 + 1e-3,
        "the transform B draws A's body from moved {largest_per_tick} units in one tick (frame \
         {at_frame}); B's lead is {lead}, which is what it used to move by"
    );
    let over_one: usize = steps.iter().filter(|(step, _)| **step > 1).map(|(_, n)| n).sum();
    assert!(
        over_one <= 20,
        "the drawn body moved by two units per tick on {over_one} frames of 100: {steps:?}"
    );
}

// ── The queue ────────────────────────────────────────────────────────────────

#[test]
fn get_or_last_holds_the_last_known_input() {
    let mut queue = InputQueue::<Input>::default();
    queue.insert(3, 7, Input::RIGHT);
    assert_eq!(queue.get_or_last(7, 7), Some(&Input::RIGHT), "tick 7 falls back to tick 3");
    assert_eq!(queue.get_or_last(3, 7), Some(&Input::RIGHT), "the tick itself");
    assert_eq!(queue.get_or_last(2, 7), None, "nothing at or before tick 2");
    assert_eq!(queue.get_or_last(7, 8), None, "another player has nothing");
    queue.insert(5, 7, Input::LEFT);
    assert_eq!(queue.get_or_last(7, 7), Some(&Input::LEFT), "the newest earlier one wins");
    assert_eq!(queue.get_or_last(4, 7), Some(&Input::RIGHT));
    let all = queue.at_tick_or_last(9);
    assert_eq!(all.get(&7), Some(&Input::LEFT));
    assert_eq!(all.len(), 1);
}

