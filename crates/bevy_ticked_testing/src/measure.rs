//! Numbers a test can put a budget on.
//!
//! Each of these measures something a consumer used to eyeball in an overlay and describe in a
//! bug report: "input feels laggy", "the join takes a while", "the snapshots are big". A number
//! with a budget in a test is what stops those from regressing quietly.
//!
//! They measure by running the network, so each one takes `&mut` and advances the session.

use std::time::Duration;

use bevy::prelude::*;
use bevy_ensemble_loopback::PeerId;
use bevy_ticked::diagnostics::TickCost;
use bevy_ticked_networking::input::TickedInput;

use crate::input::queue_input;
use crate::net::TickedNetwork;
use crate::view::tick;

/// Ticks `peer` advances per frame, averaged over `steps` frames. `1.0` is a peer keeping up;
/// a steering client sits within a couple of percent of it; a frozen or paused one reads `0`.
pub fn measure_tick_rate(net: &mut TickedNetwork, peer: PeerId, steps: usize) -> f64 {
    assert!(steps > 0, "a tick rate over zero frames is not a rate");
    let before = tick(net.app(peer));
    net.run(steps);
    let after = tick(net.app(peer));
    (after - before) as f64 / steps as f64
}

/// Frames from `peer` first holding `press` until `moved` is true of the host.
///
/// The number a player feels. `press` is held every frame until `moved` — a key held, not
/// tapped, so a frame that turns into no tick on a steering client does not lose it. `None` if
/// `patience` frames pass first, which is also what a press that changes nothing produces: the
/// measurement is only as good as `moved` is at noticing the press, and a probe that cannot see
/// the press it sends measures nothing.
///
/// # Panics
///
/// If `moved` is already true before anything is pressed. The probe would then be measuring
/// something other than the press.
pub fn measure_input_latency<I: TickedInput>(
    net: &mut TickedNetwork,
    peer: PeerId,
    press: I,
    moved: impl Fn(&mut App) -> bool,
    patience: usize,
) -> Option<u64> {
    let host = net.host();
    assert!(
        !moved(net.app_mut(host)),
        "`moved` is already true before the press: the probe is not measuring the press"
    );
    let uuid = net.uuid(peer);
    for frame in 1..=patience as u64 {
        queue_input(net.app_mut(peer), uuid, press.clone());
        net.step();
        if moved(net.app_mut(host)) {
            return Some(frame);
        }
    }
    None
}

/// What ticks have cost this app so far, replays included.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TickCostReport {
    pub ticks: u64,
    pub mean: Duration,
    pub worst: Duration,
}

/// The tick-cost counters `TickedPlugin` keeps, as a report.
pub fn measure_tick_cost(app: &App) -> TickCostReport {
    let cost = app
        .world()
        .get_resource::<TickCost>()
        .copied()
        .unwrap_or_default();
    TickCostReport {
        ticks: cost.ticks,
        mean: cost.mean(),
        worst: cost.worst,
    }
}

/// Snapshot traffic from one peer to another over a window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SnapshotSizeReport {
    pub mean_bytes: f64,
    pub max_bytes: usize,
    /// Over the window's virtual time, so it reads the way a bandwidth budget is written.
    pub bytes_per_second: f64,
    pub packets: usize,
}

/// Run `frames` frames and report the snapshots that went from `from` to `to` during them.
/// Switches tracing on if it was off.
pub fn measure_snapshot_size(
    net: &mut TickedNetwork,
    from: PeerId,
    to: PeerId,
    frames: usize,
) -> SnapshotSizeReport {
    assert!(frames > 0, "a snapshot rate over zero frames is not a rate");
    net.trace_packets();
    let start = net.net.frame();
    net.run(frames);
    let sizes: Vec<usize> = net
        .snapshots_sent(from, to)
        .into_iter()
        .filter(|snapshot| snapshot.frame > start)
        .map(|snapshot| snapshot.bytes)
        .collect();
    let total: usize = sizes.iter().sum();
    let seconds = net.net.frame_duration().as_secs_f64() * frames as f64;
    SnapshotSizeReport {
        mean_bytes: if sizes.is_empty() {
            0.0
        } else {
            total as f64 / sizes.len() as f64
        },
        max_bytes: sizes.iter().copied().max().unwrap_or(0),
        bytes_per_second: total as f64 / seconds,
        packets: sizes.len(),
    }
}

/// How many ticks `peer` is behind the host. Negative when it leads, which is where a client
/// belongs; positive is a client whose inputs are arriving late.
pub fn trails_host_by(net: &TickedNetwork, peer: PeerId) -> i64 {
    tick(net.app(net.host())) as i64 - tick(net.app(peer)) as i64
}
