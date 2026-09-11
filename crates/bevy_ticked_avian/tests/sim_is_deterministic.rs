//! Source-text guardrail: nothing in this crate reads the frame from inside the tick.

use bevy_ticked_testing::source_guard::{DEFAULT_NEEDLES, SourceGuard};

fn guard() -> SourceGuard {
    SourceGuard::new(env!("CARGO_MANIFEST_DIR"))
        .sources(&["src"])
        .ban(DEFAULT_NEEDLES)
        .min_files(1)
}

#[test]
fn the_tick_does_not_read_the_frame() {
    guard().assert_clean();
}

#[test]
fn every_exception_is_still_needed() {
    guard().assert_every_exception_is_still_needed();
}
