//! The two controllers that write a client's tick buffer run in a fixed order.
//!
//! `AdaptiveTickBufferPlugin` sizes `client_tick_buffer` from the ping round trip, and the
//! session's `size_buffer_from_margin` sizes it from the host's report of how early this client's
//! actions arrive. Both write it every frame in `Update`, and with nothing ordering them the
//! multithreaded executor ran them whichever way round it happened to: the same session on the
//! same link with the same seed, run twice in one process, settled on different buffers and
//! simulated different ticks. Found by a game whose replay test began failing when an unrelated
//! system left its schedule and stopped holding the two apart by accident.
//!
//! A race is not something a run can be relied on to show, so this asks the schedule instead.

mod common;

use bevy::prelude::*;
use common::{Recipe, peer_with};

#[test]
fn the_buffer_controllers_are_not_left_to_the_executor() {
    let mut app = peer_with(
        2,
        Recipe {
            adaptive: true,
            ..Recipe::default()
        },
    );
    app.finish();
    app.cleanup();
    // Builds every schedule, and with it the list of conflicting pairs nothing orders.
    app.update();

    let schedules = app.world().resource::<Schedules>();
    let update = schedules.get(Update).expect("an app has an Update schedule");
    let name = |key| {
        update
            .systems()
            .expect("the schedule has been built")
            .find(|(candidate, _)| *candidate == key)
            .map(|(_, system)| system.name().to_string())
            .unwrap_or_default()
    };
    let racing: Vec<(String, String)> = update
        .graph()
        .conflicting_systems()
        .0
        .iter()
        .map(|(left, right, _)| (name(*left), name(*right)))
        .filter(|(left, right)| {
            let pair = [left.as_str(), right.as_str()];
            pair.iter().any(|name| name.contains("adapt_tick_buffer"))
                && pair.iter().any(|name| name.contains("size_buffer_from_margin"))
        })
        .collect();
    assert!(
        racing.is_empty(),
        "both buffer controllers write the client's buffer and nothing orders them: {racing:?}"
    );
}
