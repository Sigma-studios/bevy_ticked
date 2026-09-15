//! What a session does when one peer's frames stop for a while.
//!
//! A browser tab in the background gets one frame a second, or none. On the client that means
//! its prediction lead is gone when it comes back; on the host it means every client keeps
//! ticking into a future the host has not produced. Both cases are a few seconds of a real
//! session and were, before this suite, a few minutes of visible wrongness afterwards.

use bevy_ticked::prelude::{TickHoldReason, TickHolds};
use bevy_ticked_networking::pause::SessionPause;
use bevy_ticked_testing::fixtures::minimal::{self, Input, seat_everyone};
use bevy_ticked_testing::prelude::*;

const SETTLE: usize = 400;
const TWO_SECONDS: usize = 128;

fn session() -> TickedNetwork {
    session_on(Link::cable())
}

fn session_on(link: Link) -> TickedNetwork {
    let mut net = TickedNetwork::client_server::<Input>(1, minimal::install)
        .with_link(link)
        .with_seed(11);
    assert!(net.settle(SETTLE));
    seat_everyone(&mut net);
    net.run(60);
    net
}

fn paused(app: &bevy::prelude::App) -> bool {
    app.world().resource::<SessionPause>().0.is_some()
}

/// After two seconds without a frame the client is behind the host. Coming back, it must get
/// its lead back within a second, and its tick must never be seen going backwards from one
/// frame to the next: the rollback happens inside a frame, not across them.
#[test]
fn a_client_alt_tab_reacquires_its_lead_without_a_visible_rewind() {
    let mut net = session();
    let (host, client) = (net.host(), net.client());
    let target = target_replay_distance(net.app(client)) as i64;

    net.freeze(client, TWO_SECONDS);
    assert!(
        lead(net.app(client), net.app(host)) < 0,
        "after two frozen seconds the client is behind the host"
    );

    let mut last_tick = tick(net.app(client));
    let mut frames_to_recover = None;
    for frame in 0..64 {
        net.step();
        let now = tick(net.app(client));
        assert!(
            now >= last_tick,
            "frame {frame}: the client's tick went from {last_tick} to {now}; a rewind was \
             visible across a frame"
        );
        last_tick = now;
        let lead = lead(net.app(client), net.app(host));
        if frames_to_recover.is_none() && (target..=target + 2).contains(&lead) {
            frames_to_recover = Some(frame);
        }
    }
    let lead = lead(net.app(client), net.app(host));
    assert!(
        (target..=target + 2).contains(&lead),
        "one second after coming back the client leads by {lead}, target {target}"
    );
    println!("lead back within {frames_to_recover:?} frames");
}

/// While the host's frames stop, clients keep running — a player is never frozen by a link
/// that went quiet, and nothing on screen would say why — and when the host comes back,
/// nobody is left holding a two-second lead to shed.
#[test]
fn a_host_alt_tab_auto_pauses_and_no_lead_piles_up() {
    let mut net = session();
    let (host, client) = (net.host(), net.client());
    let target = target_replay_distance(net.app(client)) as i64;
    let before = tick(net.app(client));

    net.freeze(host, TWO_SECONDS);
    // While the host produced nothing the client heard nothing, and kept running: its own
    // input still moves it, at the cost of a lead it will give back. The soft hold that used
    // to stop it here after a quarter second is off by default.
    let piled = lead(net.app(client), net.app(host));
    assert!(
        piled > target,
        "the client ran on through the silence: lead {piled}, target {target}"
    );
    assert!(
        tick(net.app(client)) >= before + TWO_SECONDS as u64 - 2,
        "and its clock never stopped"
    );
    assert!(
        !net.app(client).world().resource::<TickHolds>().is_held(),
        "nothing holds it: {:?}",
        net.app(client).world().resource::<TickHolds>()
    );

    // The host's first frame back is two seconds long: it pauses the session at the tick it
    // is still on, every client rolls back to it and forgets what it predicted, and the next
    // frame resumes. The lead is then re-acquired forward, not shed.
    net.run(64);
    let lead = lead(net.app(client), net.app(host));
    assert!(
        (target..=target + 2).contains(&lead),
        "one second after the host came back the client leads by {lead}, target {target}"
    );
    assert!(
        !net.app(client).world().resource::<TickHolds>().is_held(),
        "and nothing holds the client any more: {:?}",
        net.app(client).world().resource::<TickHolds>()
    );
}

/// A host frame too short to be a stall in the pause policy's eyes and too long for the
/// clients to ignore: 30 frames, under the 500 ms auto-pause. The host catches up what
/// `MaxTicksPerFrame` allows and drops the rest, so every client that kept running is now
/// that many ticks ahead of it for good. It used to freeze at a quarter second of silence and
/// then shed the rest at two percent a second; now it runs through the silence and gives the
/// excess back in one rewind, and its lead is on target within a few frames of the host's
/// return.
#[test]
fn a_short_host_stall_is_one_rewind_not_a_freeze() {
    let mut net = session();
    let (host, client) = (net.host(), net.client());
    // A player, not a spectator: input every tick, as a game's input plugin files it whether
    // or not a key is down. That is what the host times, and the rewind reads the host's
    // report to tell a host that stood still from a link that slowed.
    net.hold_input(client, Input::NONE, 64);
    let target = target_replay_distance(net.app(client)) as i64;
    let before = tick(net.app(client));

    net.freeze_while_held(host, 30, client, Input::NONE);
    assert!(
        tick(net.app(client)) >= before + 28,
        "the client kept ticking through the stall"
    );
    assert!(
        !net.app(client).world().resource::<TickHolds>().is_held(),
        "and was never held: {:?}",
        net.app(client).world().resource::<TickHolds>()
    );
    let piled = lead(net.app(client), net.app(host));
    assert!(
        piled >= target + 8,
        "the host dropped what it could not catch up, so the client is {piled} ahead of a \
         target of {target}"
    );

    let mut rewinds_seen = 0;
    let mut last_tick = tick(net.app(client));
    for frame in 0..32 {
        net.hold_input(client, Input::NONE, 1);
        assert!(
            !paused(net.app(host)),
            "frame {frame}: a short stall is not a pause"
        );
        assert!(
            !net.app(client)
                .world()
                .resource::<TickHolds>()
                .holds(TickHoldReason::SoftHold),
            "frame {frame}: the client never soft-held"
        );
        let now = tick(net.app(client));
        if now < last_tick {
            rewinds_seen += 1;
        }
        last_tick = now;
    }
    let stats = replays(net.app(client));
    assert_eq!(
        stats.snapped_back, 1,
        "exactly one rewind gave the excess back: {stats:?}"
    );
    assert_eq!(
        rewinds_seen, 1,
        "and it was the one jump seen across frames"
    );
    // The rewind lands on the target the client had at that moment. The target itself is
    // still settling — the margin the host measured on inputs sent while it stood still is
    // not a measurement — so what is left is the rate trim's, and it is nowhere near another
    // rewind's worth.
    let target_now = target_replay_distance(net.app(client)) as i64;
    let excess = stats.last_replay_distance - target_now;
    assert!(
        excess.abs() < 8,
        "half a second after the host's return the replay distance is \
         {} against a target of {target_now}",
        stats.last_replay_distance
    );
    assert!(
        !net.app(client).world().resource::<TickHolds>().is_held(),
        "and nothing holds it: {:?}",
        net.app(client).world().resource::<TickHolds>()
    );
    // And the trim closes it, with no second rewind.
    net.hold_input(client, Input::NONE, 192);
    let target = target_replay_distance(net.app(client)) as i64;
    let lead = lead(net.app(client), net.app(host));
    assert!(
        (target..=target + 2).contains(&lead),
        "three seconds later the client leads by {lead}, target {target}"
    );
    assert_eq!(replays(net.app(client)).snapped_back, 1);
}

/// The rewind is for a host whose clock fell behind, not for a link whose packets arrive
/// late. Bad Wi-Fi — 40 ms of jitter on a 60 ms trip, two percent loss, duplicates — for
/// twenty seconds of play never triggers it, because the client applies only the newest
/// snapshot it holds each frame and a late burst is followed by a fresh one.
#[test]
fn a_jittery_link_never_triggers_the_rewind() {
    for link in [Link::four_g(), Link::bad_wifi()] {
        let mut net = session_on(link);
        let (host, client) = (net.host(), net.client());
        for _ in 0..(20 * 64) {
            net.hold_input(client, Input::NONE, 1);
            assert!(
                !net.app(client)
                    .world()
                    .resource::<TickHolds>()
                    .holds(TickHoldReason::SoftHold),
                "the client never soft-held on a live link"
            );
        }
        let stats = replays(net.app(client));
        assert_eq!(
            stats.snapped_back, 0,
            "jitter read as a stalled host: {stats:?}"
        );
        let target = target_replay_distance(net.app(client)) as i64;
        let lead = lead(net.app(client), net.app(host));
        assert!(
            (1..=target + 8).contains(&lead),
            "and the lead is steered, neither lost nor piled up: {lead} against a target \
             of {target}"
        );
    }
}

/// A client that sends no input — nothing to drive, a menu up — gives the host nothing to time,
/// and the host must say so rather than report a margin of zero. It used to: the client took
/// the zero for "on time", set its target two ticks past its lead, chased it at the trim's full
/// rate, and reached the 64-tick ceiling in under a minute — a second of replay on every
/// snapshot, four frames of holding for each, for a player who had touched nothing.
#[test]
fn an_idle_client_does_not_ratchet_its_lead() {
    let mut net = session();
    let (host, client) = (net.host(), net.client());
    let target_at_start = target_replay_distance(net.app(client));
    for second in 0..20 {
        net.run(64);
        let target = target_replay_distance(net.app(client));
        let replay = replays(net.app(client)).last_replay_distance;
        assert!(
            target <= target_at_start + 2,
            "{second}s in, the target has drifted from {target_at_start} to {target} with no \
             input sent"
        );
        assert!(
            replay <= target as i64 + 2,
            "{second}s in, the replay distance is {replay} against a target of {target}"
        );
    }
    assert!(
        !net.app(client).world().resource::<TickHolds>().is_held(),
        "{:?}",
        net.app(client).world().resource::<TickHolds>()
    );
    let _ = host;
}
