//! Source-text guardrails on the lockstep crate and its example.
//!
//! Lockstep is the mode with no rollback to hide behind: every peer runs every tick once, from
//! the same actions, and the checksum exchange is the only thing that notices when they do not
//! agree. So nothing in `src/` may read the frame. The reaches it has are on the frame side —
//! the adaptive buffer turning a measured latency into a tick count, and a `#[cfg(test)]` helper
//! stamping a synthetic packet — and are listed as such.
//!
//! The example is a whole game in one file, so its keyboard and sprites live next to its ticked
//! system and the guard cannot tell them apart by schedule; each is listed with the Update
//! system it belongs to.

use bevy_ticked_testing::source_guard::{DEFAULT_NEEDLES, Exception, SourceGuard};

fn guard() -> SourceGuard {
    SourceGuard::new(env!("CARGO_MANIFEST_DIR"))
        .sources(&["src", "examples"])
        .ban(DEFAULT_NEEDLES)
        .allow(&[
            Exception {
                path: "src/adaptive_buffer.rs",
                needle: "SECONDS_PER_TICK",
                reason: "turns a measured round trip into a number of ticks of input buffer; \
                         runs in Update and reads a measurement, never the tick's state",
                expires: None,
            },
            Exception {
                path: "src/checksum_exchange.rs",
                needle: "Instant::now",
                reason: "a #[cfg(test)] helper stamps `received_at` on a synthetic checksum \
                         report; the stamp is transport metadata and never enters the tick",
                expires: None,
            },
            Exception {
                path: "examples/block_placer.rs",
                needle: "ButtonInput",
                reason: "the lobby keys and `capture_local_input` run in Update; the tick \
                         receives actions, never the keyboard",
                expires: None,
            },
            Exception {
                path: "examples/block_placer.rs",
                needle: "Sprite",
                reason: "sprites are attached in Update by `attach_visuals` to the bodies the \
                         tick spawned bare; `spawn_joined_players` and `apply_actions` in the \
                         tick write only game state",
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
