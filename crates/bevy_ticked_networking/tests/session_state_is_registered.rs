//! Source-text guardrail: no system in this crate keeps state in a `Local`.
//!
//! See [`SESSION_STATE_NEEDLES`] for why. The one exception is per-call scratch, cleared at the
//! top of every call, which holds nothing from one call to the next and so nothing from one
//! session to the next.

use bevy_ticked_testing::source_guard::{Exception, SESSION_STATE_NEEDLES, SourceGuard};

fn guard() -> SourceGuard {
    SourceGuard::new(env!("CARGO_MANIFEST_DIR"))
        .sources(&["src"])
        .ban(SESSION_STATE_NEEDLES)
        .allow(&[Exception {
            path: "src/smoothing.rs",
            needle: "Local<",
            reason: "`remember_before` reuses a map's allocation as scratch: it is cleared at the \
                     top of every call and taken whole into a resource at the end, so it holds \
                     nothing between calls",
            expires: None,
        }])
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

#[test]
fn every_exception_is_still_needed() {
    guard().assert_every_exception_is_still_needed();
}

#[test]
fn no_exception_has_expired() {
    guard().assert_no_exception_has_expired();
}
