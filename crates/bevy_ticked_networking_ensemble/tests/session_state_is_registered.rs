//! Source-text guardrail: no system in the ensemble bridge keeps state in a `Local`.
//!
//! See [`SESSION_STATE_NEEDLES`] for why. The exceptions are the net-debug overlay, whose rates
//! are presentation over process-lifetime counters, and a test helper that burns time.

use bevy_ticked_testing::source_guard::{Exception, SESSION_STATE_NEEDLES, SourceGuard};

fn guard() -> SourceGuard {
    SourceGuard::new(env!("CARGO_MANIFEST_DIR"))
        .sources(&["src"])
        .ban(SESSION_STATE_NEEDLES)
        .allow(&[
            Exception {
                path: "src/overlay.rs",
                needle: "Local<",
                reason: "`publish_ticked_lines` keeps the previous frame's counters to turn them \
                         into per-second rates for the overlay: diagnostics over counters that \
                         live as long as the app, never read back by a session",
                expires: None,
            },
            Exception {
                path: "src/lib.rs",
                needle: "Local<",
                reason: "`busy`, a #[cfg(test)] system that only burns time to displace an \
                         exclusive one in the executor; its accumulator means nothing",
                expires: None,
            },
        ])
        .min_files(4)
}

#[test]
fn the_guardrail_bites() {
    guard().assert_it_bites();
}

#[test]
fn no_system_keeps_session_state_in_a_local() {
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
