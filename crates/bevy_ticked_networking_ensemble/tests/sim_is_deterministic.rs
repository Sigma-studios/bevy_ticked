//! Source-text guardrails on the ensemble transport crate and its examples.
//!
//! `src/` is transport: it moves snapshots and inputs between peers and never runs inside the
//! tick, but it is also the crate a game copies its plumbing from, so it holds to the same rule.
//! The only reaches are two `#[cfg(test)]` helpers stamping a synthetic packet's `received_at`.
//!
//! The examples are whole games in one file each, so their input capture, sprites and lobby keys
//! live next to their ticked systems and the guard cannot tell them apart by schedule. Each is
//! listed with the frame-side system it belongs to. The examples integrate from `Res<Time>`,
//! which `bevy_ticked` sets to the tick clock inside a tick.

use bevy_ticked_testing::source_guard::{DEFAULT_NEEDLES, Exception, SourceGuard};

fn guard() -> SourceGuard {
    SourceGuard::new(env!("CARGO_MANIFEST_DIR"))
        .sources(&["src", "examples"])
        .ban(DEFAULT_NEEDLES)
        .allow(&[
            Exception {
                path: "src/lib.rs",
                needle: "Instant::now",
                reason: "a #[cfg(test)] helper stamps `received_at` on a synthetic snapshot \
                         packet; the stamp is transport metadata and never enters the tick",
                expires: None,
            },
            Exception {
                path: "src/handshake.rs",
                needle: "Instant::now",
                reason: "a #[cfg(test)] helper stamps `received_at` on a synthetic handshake \
                         message; the stamp is transport metadata and never enters the tick",
                expires: None,
            },
            Exception {
                path: "src/overlay.rs",
                needle: "Res<Time<Real>",
                reason: "`publish_ticked_lines` runs in PostUpdate and turns the tick counters \
                         into per-second rates for the net-debug overlay; a rate is a \
                         wall-clock quantity and nothing here is read back by the tick",
                expires: None,
            },
            Exception {
                path: "src/overlay.rs",
                needle: "Time<Real>",
                reason: "the same PostUpdate overlay system; see the `Res<Time<Real>` entry",
                expires: None,
            },
            Exception {
                path: "src/local_session.rs",
                needle: "Res<Time<Real>",
                reason: "`drive_auto_role` runs in Update and refreshes the lobby list once a \
                         second of wall clock until a lobby is listed; it never runs in the tick",
                expires: None,
            },
            Exception {
                path: "src/local_session.rs",
                needle: "Time<Real>",
                reason: "the same Update system; see the `Res<Time<Real>` entry",
                expires: None,
            },
            Exception {
                path: "examples/netpeer.rs",
                needle: "Instant::now",
                reason: "the process runner paces `update()` to a tick's worth of wall clock, \
                         bounds the session by a deadline and stamps the pulse; all of it is \
                         outside `TickedLoop`, and the simulation is the fixture's",
                expires: None,
            },
            Exception {
                path: "examples/fps_shooter.rs",
                needle: "ButtonInput",
                reason: "the lobby keys and `capture_local_input` run in Update; the tick \
                         receives a PlayerInput component, never the keyboard",
                expires: None,
            },
            Exception {
                path: "examples/top_down_shooter.rs",
                needle: "ButtonInput",
                reason: "the lobby keys and `capture_local_input` run in Update; the tick \
                         receives a PlayerInput component, never the keyboard",
                expires: None,
            },
            Exception {
                path: "examples/top_down_shooter.rs",
                needle: "Sprite",
                reason: "sprites are spawned by `setup` and the `on_entity_spawned` observer \
                         and moved by `sync_visuals`, all on the frame side; the tick moves \
                         Position, not Transform",
                expires: None,
            },
        ])
        .min_files(5)
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
