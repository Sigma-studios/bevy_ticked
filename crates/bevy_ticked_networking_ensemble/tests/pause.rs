//! A pause for everybody, on the authority's word, and what each peer does with it.

use bevy_ticked::prelude::{TickHoldReason, TickHolds};
use bevy_ticked_networking::pause::{
    PausePolicy, PauseReason, PauseSession, ResumeSession, SessionPause, WhoMayPause,
};
use bevy_ticked_testing::fixtures::minimal::{self, Input, Pos, seat_everyone};
use bevy_ticked_testing::prelude::*;

const SETTLE: usize = 400;

fn session(clients: usize) -> TickedNetwork {
    let mut net = TickedNetwork::client_server::<Input>(clients, minimal::install)
        .with_link(Link::cable())
        .with_seed(3);
    assert!(net.settle(SETTLE));
    seat_everyone(&mut net);
    net.run(60);
    net
}

fn holds(app: &bevy::prelude::App) -> &TickHolds {
    app.world().resource::<TickHolds>()
}

fn pause_of(app: &bevy::prelude::App) -> Option<u64> {
    app.world().resource::<SessionPause>().0.map(|p| p.at)
}

#[test]
fn pausing_the_session_holds_every_peer_at_the_same_tick() {
    let mut net = session(2);
    let host = net.host();
    net.app_mut(host)
        .world_mut()
        .write_message(PauseSession(PauseReason::Host));
    net.run(30);

    let at = pause_of(net.app(host)).expect("the host is paused");
    for peer in net.peers() {
        assert_eq!(
            pause_of(net.app(peer)),
            Some(at),
            "peer {peer:?} knows the pause"
        );
        assert_eq!(
            tick(net.app(peer)),
            at,
            "peer {peer:?} holds at the paused tick"
        );
        assert!(holds(net.app(peer)).holds(TickHoldReason::SessionPause));
    }
    // And stays there.
    net.run(30);
    for peer in net.peers() {
        assert_eq!(tick(net.app(peer)), at);
    }
}

#[test]
fn a_client_ahead_of_paused_at_rolls_back_to_it_and_forgets_its_prediction() {
    let mut net = session(1);
    let (host, client) = (net.host(), net.client());
    let seats = seat_everyone(&mut net);
    let mine = seats
        .iter()
        .find(|(uuid, _)| *uuid == net.uuid(client))
        .map(|(_, id)| *id)
        .unwrap();
    net.run(40);
    // The client is walking, ahead of the host by its lead.
    net.hold_input(client, Input::RIGHT, 20);
    let before = latest::<Pos>(net.app(client), mine).unwrap().0;
    net.app_mut(host)
        .world_mut()
        .write_message(PauseSession(PauseReason::Host));
    net.hold_input(client, Input::RIGHT, 20);

    let at = pause_of(net.app(host)).unwrap();
    assert_eq!(tick(net.app(client)), at, "rolled back to the paused tick");
    let shown = latest::<Pos>(net.app(client), mine).unwrap().0;
    let host_has = latest::<Pos>(net.app(host), mine).unwrap().0;
    assert_eq!(
        shown, host_has,
        "the body is where the host has it, not where it was predicted"
    );
    assert!(
        shown <= before + 20,
        "nothing predicted past the pause survives"
    );
}

#[test]
fn resuming_reacquires_the_lead_without_a_replay_burst() {
    let mut net = session(1);
    let (host, client) = (net.host(), net.client());
    let target = target_replay_distance(net.app(client)) as i64;
    net.app_mut(host)
        .world_mut()
        .write_message(PauseSession(PauseReason::Host));
    net.run(30);
    let stats_before = replays(net.app(client));
    net.app_mut(host).world_mut().write_message(ResumeSession);
    net.run(4);
    let lead = lead(net.app(client), net.app(host));
    assert!(
        (target - 2..=target + 2).contains(&lead),
        "four frames after resuming the client leads by {lead}, target {target}"
    );
    let stats = replays(net.app(client));
    assert!(
        stats.ticks_replayed - stats_before.ticks_replayed <= target as u64 + 4,
        "the lead was taken forward, not replayed: {stats:?}"
    );
    assert!(!holds(net.app(client)).is_held());
    net.run(60);
    assert!(pause_of(net.app(client)).is_none());
}

#[test]
fn a_large_time_real_gap_on_the_host_auto_pauses() {
    let mut net = session(1);
    let (host, client) = (net.host(), net.client());
    net.freeze(host, 64);
    net.step();
    let pause = net.app(host).world().resource::<SessionPause>().0;
    assert!(
        pause.is_some_and(|p| p.reason == PauseReason::HostStalled),
        "a one-second frame is a stall: {pause:?}"
    );
    net.run(2);
    assert!(
        pause_of(net.app(host)).is_none(),
        "and it resumes on its own the next frame"
    );
    let _ = client;
}

#[test]
fn a_client_with_no_snapshot_for_n_ms_enters_soft_hold() {
    let mut net = session(1);
    let (host, client) = (net.host(), net.client());
    net.freeze(host, 8);
    assert!(
        !holds(net.app(client)).holds(TickHoldReason::SoftHold),
        "an eighth of a second is a hiccup"
    );
    net.freeze(host, 24);
    assert!(
        holds(net.app(client)).holds(TickHoldReason::SoftHold),
        "half a second of silence and the client stops running ahead"
    );
    net.run(8);
    assert!(
        !holds(net.app(client)).holds(TickHoldReason::SoftHold),
        "the first snapshot releases it"
    );
}

#[test]
fn a_client_pause_request_is_ignored_under_host_only_policy() {
    let mut net = session(1);
    let (host, client) = (net.host(), net.client());
    net.app_mut(client)
        .world_mut()
        .write_message(PauseSession(PauseReason::Custom(1)));
    net.run(20);
    assert_eq!(
        pause_of(net.app(host)),
        None,
        "the host did not pause for a client"
    );
    assert!(!holds(net.app(client)).is_held());

    net.app_mut(host).insert_resource(PausePolicy {
        who_may_pause: WhoMayPause::AnyParticipant,
        ..PausePolicy::default()
    });
    net.app_mut(client)
        .world_mut()
        .write_message(PauseSession(PauseReason::Custom(1)));
    net.run(20);
    let pause = net
        .app(host)
        .world()
        .resource::<SessionPause>()
        .0
        .expect("honoured now");
    assert_eq!(pause.reason, PauseReason::Participant(net.uuid(client)));
    assert_eq!(pause_of(net.app(client)), Some(pause.at));
    net.app_mut(client).world_mut().write_message(ResumeSession);
    net.run(20);
    assert_eq!(pause_of(net.app(host)), None, "and a resume request too");
}
