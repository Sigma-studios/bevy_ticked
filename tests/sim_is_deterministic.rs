//! Source-text guardrails on the core crate: nothing that runs in the tick reads the frame.
//!
//! `bevy_ticked` is the crate that *installs* the tick clock, so it is also the one crate whose
//! source legitimately names `Time<Virtual>` and `Time<Fixed>`: something has to feed the
//! accumulator from the frame and copy the fixed timestep into `Time<Ticked>`. Those are the
//! driver, listed below as exceptions with the system each one is, and `src/time.rs` — the
//! swap of `Time` for the tick's duration and the `Instant` that times it — is excluded as a
//! whole. Everything else in `src/` and the example holds to the same rule as a game.

use bevy_ticked_testing::source_guard::{DEFAULT_NEEDLES, Exception, SourceGuard};

fn guard() -> SourceGuard {
    SourceGuard::new(env!("CARGO_MANIFEST_DIR"))
        .sources(&["src", "examples"])
        .not_ticked(&[
            // The clock driver: builds `Time<Ticked>` for the tick about to run, swaps it in as
            // `Time`, runs the schedule against `Instant::now` for `TickCost`, and swaps back.
            // It is the reason no other file needs to name a clock.
            "src/time.rs",
        ])
        .ban(DEFAULT_NEEDLES)
        .allow(&[
            Exception {
                path: "src/lib.rs",
                needle: "Res<Time<Fixed>",
                reason: "`mirror_fixed_clock` copies the fixed timestep and overstep into \
                         Time<Ticked> once per frame, after the last fixed step; it is the \
                         driver, not a ticked system",
                expires: None,
            },
            Exception {
                path: "src/lib.rs",
                needle: "ResMut<Time<",
                reason: "`mirror_fixed_clock` writes Time<Ticked>; the driver is the one writer \
                         of the tick clock",
                expires: None,
            },
            Exception {
                path: "src/lib.rs",
                needle: "Time<Virtual>",
                reason: "`drive_ticked_loop_from_accumulator` feeds the tick accumulator from \
                         the virtual clock's delta; this is the one place the frame decides how \
                         many ticks run, and it runs outside the tick",
                expires: None,
            },
            Exception {
                path: "src/tick.rs",
                needle: "SECONDS_PER_TICK",
                reason: "the definition; ticked systems read Res<Time>::delta(), which the \
                         driver sets from Time<Fixed>, and the constant stays for frame-side \
                         configuration",
                expires: None,
            },
            Exception {
                path: "src/prelude.rs",
                needle: "SECONDS_PER_TICK",
                reason: "the re-export of the definition in src/tick.rs",
                expires: None,
            },
            Exception {
                path: "examples/bouncing_ball.rs",
                needle: "ButtonInput",
                reason: "`keyboard_controls` runs in Update and pauses, resumes and steps the \
                         tick; the keys never reach a ticked system",
                expires: None,
            },
        ])
        .min_files(10)
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

#[test]
fn the_excluded_paths_still_exist() {
    guard().assert_excluded_paths_exist();
}
