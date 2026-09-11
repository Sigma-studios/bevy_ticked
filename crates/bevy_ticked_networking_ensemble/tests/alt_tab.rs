//! What a session does when one peer's frames stop for a while.
//!
//! A browser tab in the background gets one frame a second, or none. On the client that means
//! its prediction lead is gone when it comes back; on the host it means every client keeps
//! ticking into a future the host has not produced. Both cases are a few seconds of a real
//! session and were, before this suite, a few minutes of visible wrongness afterwards.

use bevy_ticked::prelude::{TickHoldReason, TickHolds};
use bevy_ticked_testing::fixtures::minimal::{self, Input, seat_everyone};
use bevy_ticked_testing::prelude::*;

const SETTLE: usize = 400;
const TWO_SECONDS: usize = 128;

fn session() -> TickedNetwork {
    let mut net = TickedNetwork::client_server::<Input>(1, minimal::install)
        .with_link(Link::cable())
        .with_seed(11);
    assert!(net.settle(SETTLE));
    seat_everyone(&mut net);
    net.run(60);
    net
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

/// While the host's frames stop, clients must not run ahead into ticks the host will never
/// confirm; when it comes back, nobody should be holding a two-second lead to shed.
#[test]
fn a_host_alt_tab_auto_pauses_and_no_lead_piles_up() {
    let mut net = session();
    let (host, client) = (net.host(), net.client());
    let target = target_replay_distance(net.app(client)) as i64;

    net.freeze(host, TWO_SECONDS);
    // While the host produced nothing the client heard nothing, and after a quarter of a
    // second of silence it held its own clock rather than run into a future the host was not
    // making. The audit measured about 1900 ticks of excess lead here, shed at 1.28 a second.
    let piled = lead(net.app(client), net.app(host));
    assert!(
        piled <= target + 24,
        "the client ran {piled} ticks ahead of a host that produced none"
    );
    assert!(
        net.app(client)
            .world()
            .resource::<TickHolds>()
            .holds(TickHoldReason::SoftHold),
        "the client is holding on its own"
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
