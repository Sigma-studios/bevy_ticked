//! The documented bundle replays bit-identically; the settings it turns off are the ones that
//! would break that; and the trace it produces today is the trace it produces tomorrow.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

use bevy_ticked_testing::fixtures::avian::{self, AvianHash, sleeping_bodies, spawn_stack};
use bevy_ticked_testing::golden::{check_golden, record_trace};
use bevy_ticked_testing::prelude::*;

const BOXES: usize = 6;

/// A stack of boxes toppling under the bundle: a rollback to any tick and a replay from it
/// reproduces every bit of every body.
#[test]
fn a_stack_of_boxes_replays_bit_identically_with_the_documented_bundle() {
    let mut app = peer_app(HOST_UUID, avian::install);
    spawn_stack(app.world_mut(), BOXES);
    // Mid-topple: bodies in contact, rotating, sliding.
    assert_replays_identically::<AvianHash>(&mut app, 40, 96);
}

/// The negative control. With sleeping allowed, a body that fell asleep during the live run
/// is asleep when the replay begins from a tick where it was still moving: the solver skips
/// it, the hashes part, and the check says so.
#[test]
fn sleeping_is_disabled_for_tracked_bodies_or_the_replay_diverges() {
    let mut app = peer_app(HOST_UUID, avian::install_sleepy);
    spawn_stack(app.world_mut(), BOXES);
    // Run until the first body sleeps, then replay across that moment.
    let mut frames = 0;
    while sleeping_bodies(app.world_mut()) == 0 {
        app.update();
        frames += 1;
        assert!(frames < 4096, "no body slept in {frames} frames");
    }
    let slept_at = tick(&app);
    println!("first body asleep at tick {slept_at}");
    // The replay spans the sleep: from before it, through it, past it. Not from tick 0,
    // which the harness cannot start a replay at: with warm starting the stack settles
    // early enough that 64 ticks before the first sleep would be.
    let from = slept_at.saturating_sub(64).max(1);
    // A second app run to the same point, so the check starts at `from`.
    let mut fresh = peer_app(HOST_UUID, avian::install_sleepy);
    spawn_stack(fresh.world_mut(), BOXES);
    let diverged = catch_unwind(AssertUnwindSafe(|| {
        assert_replays_identically::<AvianHash>(&mut fresh, from, 128);
    }))
    .is_err();
    assert!(
        diverged,
        "with sleeping allowed the replay agreed with the live run across the sleep at tick \
         {slept_at}; the negative control proves nothing"
    );

    // And with the bundle as documented, the same window replays cleanly.
    let mut bundled = peer_app(HOST_UUID, avian::install);
    spawn_stack(bundled.world_mut(), BOXES);
    assert_replays_identically::<AvianHash>(&mut bundled, from, 128);
}

/// The stack's trace, sampled every eight ticks for four seconds, matches the recorded one.
/// Re-record with `UPDATE_GOLDEN=1` and read the diff.
#[test]
fn the_avian_stack_golden_matches() {
    let mut app = peer_app(HOST_UUID, avian::install);
    spawn_stack(app.world_mut(), BOXES);
    let trace = record_trace::<AvianHash>(&mut app, 256, 8);
    check_golden(
        Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/golden/avian_stack.trace"
        )),
        &trace,
    );
}
