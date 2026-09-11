//! Source-text guardrails on the networking crate: nothing in it reads the frame.
//!
//! This crate applies snapshots and replays ticks. A replayed tick that read the wall clock, the
//! frame delta or the keyboard would produce a different answer from the tick it replaced, which
//! is the definition of a desync — and it would do so only on the peer that rolled back, which
//! is the most expensive place to find anything. So the whole of `src/` holds to the rule a
//! game's simulation holds to, with one exception, listed with its reason;
//! `every_exception_is_still_needed` fails when it stops being used.

use bevy_ticked_testing::source_guard::{DEFAULT_NEEDLES, Exception, SourceGuard};

fn guard() -> SourceGuard {
    SourceGuard::new(env!("CARGO_MANIFEST_DIR"))
        .sources(&["src"])
        .ban(DEFAULT_NEEDLES)
        .allow(&[
            Exception {
                path: "src/pause.rs",
                needle: "Res<Time<Real>",
                reason: "the host's stall detector and the client's soft hold measure wall \
                         time between frames, in `PreUpdate`, outside every tick: a stall is \
                         a wall-clock fact and nothing else could see it",
                expires: None,
            },
            Exception {
                path: "src/pause.rs",
                needle: "Time<Real>",
                reason: "the same two frame-side systems name the type in their signatures",
                expires: None,
            },
        ])
        .min_files(3)
}

#[test]
fn the_guardrail_bites() {
    guard().assert_it_bites();
}

#[test]
fn the_tick_does_not_read_the_frame() {
    guard().assert_clean();
}

#[test]
fn every_exception_is_still_needed() {
    guard().assert_every_exception_is_still_needed();
}

#[test]
fn no_exception_has_expired() {
    guard().assert_no_exception_has_expired();
}
