//! Source-text guardrail: no system in the core crate keeps state in a `Local`.
//!
//! See [`SESSION_STATE_NEEDLES`] for why. The core had one — the "tick 0 captured" flag — and it
//! is the `InitialCapture` resource now, re-armed by every session door.

use bevy_ticked_testing::source_guard::{SESSION_STATE_NEEDLES, SourceGuard};

fn guard() -> SourceGuard {
    SourceGuard::new(env!("CARGO_MANIFEST_DIR"))
        .sources(&["src"])
        .ban(SESSION_STATE_NEEDLES)
        .min_files(10)
}

#[test]
fn the_guardrail_bites() {
    guard().assert_it_bites();
}

#[test]
fn no_system_keeps_session_state_in_a_local() {
    guard().assert_clean();
}
