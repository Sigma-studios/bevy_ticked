//! Sizing the tick buffer from the connection it is compensating for.
//!
//! The lockstep buffer is a *playout buffer*: locally-issued actions are scheduled
//! [`LockstepConfig::client_tick_buffer`] ticks in the future so every peer has received them
//! before that tick is simulated. It is the single number that decides how a session feels, and
//! a fixed one is wrong for everyone at once — too small and high-latency peers stall the whole
//! session, too large and low-latency peers eat needless input lag. The default of 6 suits a LAN
//! and nothing else.
//!
//! This controller sizes it from the measured round-trip time ([`PeerRtt`]) plus a jitter
//! headroom, so it tracks the actual connection:
//!
//! * **Client instances** adapt [`LockstepConfig::client_tick_buffer`] from the RTT to the host
//!   (stored on the lobby entity).
//! * **Host instances** adapt [`LockstepConfig::host_tick_buffer`] from the worst RTT across all
//!   connected clients (stored on the `LobbyClient` entities), since the host's own actions must
//!   reach the slowest peer.
//!
//! Add [`AdaptiveTickBufferPlugin`] and it runs. Leave it out and [`LockstepConfig`] keeps
//! whatever it was built with, which is what every consumer got before this existed.
//!
//! # Why the buffer grows at once and shrinks slowly
//!
//! Because the two directions cost different things. A buffer that is too small stalls the
//! session on every tick until it catches up, so growth takes the new target immediately. A
//! buffer that is too large only costs input latency, and a connection that looks better for a
//! moment usually is not, so shrinking waits out [`SHRINK_COOLDOWN_FRAMES`] and then gives back a
//! single tick.
//!
//! **It used to walk up one tick at a time as well, and that was a workaround, not a policy.** A
//! client's scheduled tick is `next_tick + buffer`, so changing `buffer` by `n` shifts the
//! sequence of scheduled ticks by `n` — and this crate handled that badly in both directions.
//! Growing left a hole: raising the buffer from 2 to 6 between two flushes made them target `T`
//! then `T+5`, nothing ever scheduled `T+1..T+4`, and the host blocked on the first of those for
//! ever. Shrinking collided: two flushes targeting the same tick, with
//! [`insert_actions_into_tracker`](crate::insert_actions_into_tracker) overwriting rather than
//! merging, silently dropped one flush's actions.
//!
//! Both are fixed. [`flush_pending_actions`](crate::flush_pending_actions) emits filler for every
//! tick from the last scheduled one onwards, so the sequence is contiguous however far the buffer
//! jumps, and the tracker merges with `.entry(tick)`. So growth is immediate, and a large latency
//! step is absorbed in a frame rather than the couple of seconds the walk used to take.
//!
//! # What it needs from a backend
//!
//! [`PeerRtt`], written often enough to be a signal. `bevy_ensemble_loopback` publishes one that
//! carries the link's *jitter* rather than its mean precisely so a controller like this one has
//! something to size headroom from; a real transport's ping does the same thing by measuring.

use bevy::prelude::*;
use bevy_ensemble::{Host, Lobby, LobbyClient, PeerRtt, PeerRttJitter};
use bevy_ticked::prelude::SECONDS_PER_TICK;

use crate::LockstepConfig;

/// Never buffer fewer than this many ticks, even on a perfect connection.
///
/// Four rather than two, and for the same reason as [`AdaptiveBufferTuning::extra_ticks`]: on a
/// link fast enough that the RTT rounds to nothing, the two ticks of scheduling overhead are the
/// *whole* cost, and a floor that does not cover them leaves a LAN game running its simulation at
/// half speed. Measured before the change: 32 Hz on a 15 ms link.
const MIN_BUFFER: u64 = 4;

/// The two numbers that decide the trade between input latency and tick rate.
///
/// Split out of the constants they used to be so they can be measured rather than argued about.
///
/// The relationship they sit on is that a peer can only advance as many ticks per round trip as
/// it can schedule ahead, so
///
/// ```text
/// ticks per second ≈ TICKS_PER_SECOND * buffer / rtt_in_ticks
/// input latency    ≈ (1 + buffer) * SECONDS_PER_TICK
/// ```
///
/// Buffer is on the top of one and the bottom of the other. There is no setting that is good at
/// both; there is only a choice about which one to spend.
#[derive(Resource, Clone, Copy, Debug)]
pub struct AdaptiveBufferTuning {
    /// Multiple of the measured RTT to aim the buffer at.
    ///
    /// Scales with the link, so it is the wrong knob for a fixed overhead and the right one for
    /// anything proportional to distance.
    pub rtt_factor: f32,
    /// Ticks added on top, flat.
    ///
    /// This was 2, and it was covering two different things — only one of which was ever real
    /// overhead.
    ///
    /// The first was a peer reading a packet a frame after it landed, because the lockstep
    /// receivers sat in `Update` while the backend drains the socket in `PreUpdate` and the tick
    /// loop runs between them. That is a whole frame in each direction and it is now gone; the
    /// receivers moved. Measured on a steady link, `extra_ticks: 0` used to fall to 48 Hz on 4G
    /// and now holds 64 Hz from LAN to satellite.
    ///
    /// The second was jitter headroom, which it supplied by accident while the term meant to
    /// supply it read a pre-smoothed series and came out at approximately zero. That term takes
    /// [`PeerRttJitter`] now, so the headroom is sized from the spread instead of from a constant
    /// that happened to be about the right size on the links somebody tried.
    ///
    /// What is left is one tick of slop, and it is deliberately not zero. The measurement says
    /// zero holds a full 64 Hz even on badly jittering links — but it is taken on
    /// `bevy_ensemble_loopback`, which runs exactly one tick per frame. A real client renders at
    /// 60 fps against a 64 Hz tick, so its frames and its ticks drift through each other, and a
    /// frame that spikes has nowhere to be absorbed. One tick is ~16 ms against a symptom that
    /// costs a visible stutter; set it to 0 if you have measured your own frame pacing.
    pub extra_ticks: u64,
    /// Hard ceiling on the buffer, in ticks.
    ///
    /// Also, implicitly, a decision that a link needing more than this is not worth playing on:
    /// past the cap the session runs slower than wall-clock and stays that way.
    pub max_buffer: u64,
}

impl Default for AdaptiveBufferTuning {
    fn default() -> Self {
        Self {
            rtt_factor: 1.0,
            // One, down from two. See the field docs: one of those two ticks was a frame of
            // scheduling latency that has been removed rather than compensated for, and the other
            // was jitter headroom now sized from a real measurement. What remains is slop for a
            // frame rate that does not divide the tick rate.
            extra_ticks: 1,
            // Past ~440 ms RTT the buffer can no longer reach the round trip and the whole
            // simulation runs slower than real time. Capping at 30 did not save the player any
            // input latency — at 1 s RTT they waited 1065 ms either way — it only decided whether
            // they waited it at 29 Hz or at 64 Hz.
            max_buffer: 96,
        }
    }
}

/// EMA smoothing for the RTT estimate. Low enough that a single spike barely moves the buffer,
/// high enough to follow a genuine latency shift within a second or so.
const RTT_ALPHA: f32 = 0.15;
/// How many multiples of the jitter estimate to add as headroom, so an occasional late packet
/// still lands before its scheduled tick.
const JITTER_SAFETY: f32 = 2.0;

/// Minimum frames between successive single-tick shrink steps. Shrinking is deliberately lazy: we
/// drop latency slowly once a connection has clearly and durably improved, rather than reacting to
/// a momentary dip.
const SHRINK_COOLDOWN_FRAMES: u32 = 120;

// Growth has no cooldown. It used to: a jump left a hole in the client's scheduled-tick sequence
// and hung the session, so the buffer had to be walked up one tick at a time. `flush_pending_actions`
// now emits filler for every tick from the last scheduled one onwards, which makes the sequence
// contiguous however far the buffer moves, so growth goes straight to the target — which is what
// it should always have done. While the buffer is too small the session stalls on every tick, and
// there is no reason to spend two seconds getting to a number already known.

/// Smoothed network estimates backing the adaptive buffer. A single app is either a host or a
/// client for the life of a lobby, so one state instance serves whichever buffer is being driven.
#[derive(Resource, Default)]
pub struct AdaptiveBufferState {
    ema_rtt: Option<f32>,
    /// The spread the transport reported, as published. Not smoothed again here — see
    /// [`update_estimates`].
    jitter: f32,
    frames_since_shrink: u32,
}

/// Drives [`LockstepConfig`] from [`PeerRtt`]. See the module docs.
pub struct AdaptiveTickBufferPlugin;

impl Plugin for AdaptiveTickBufferPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<AdaptiveBufferState>()
            .init_resource::<AdaptiveBufferTuning>()
            .add_systems(Update, adapt_tick_buffer);
    }
}

/// Fold a new RTT sample into the smoothed estimates.
///
/// Only called on frames where [`PeerRtt`] was actually written, so a high frame rate does not
/// integrate the same ping sixty times and flatten the jitter estimate toward zero.
///
/// That freshness test used to be `raw_rtt != last_raw_rtt`, which is a different question and
/// gets a different answer: it treats *a new sample that happens to equal the last one* as no
/// sample at all. On a steady link — which is most links, to the resolution a ping measures — that
/// means the very first sample is integrated and every one after it is discarded, so `ema_rtt`
/// freezes at whatever it saw first and never converges. A peer whose latency fell from five
/// seconds to ten milliseconds kept a five-second estimate, and therefore a pinned-at-maximum
/// buffer, for the rest of the session.
///
/// Bevy's change detection answers the question actually being asked, so that is what decides now.
///
/// # The jitter is taken, not derived
///
/// It used to be computed here, as an EMA of `(raw_rtt - previous_ema).abs()`. That looks like a
/// jitter estimate and is not one, because [`PeerRtt`] is *already smoothed by the transport*: its
/// variation is the smoothed signal's variation, which is the thing the smoothing removed. A
/// second EMA on top of that, fed once a second, converged to roughly zero on links with tens of
/// milliseconds of genuine spread — so `JITTER_SAFETY * jitter` contributed nothing and the buffer
/// carried no headroom at all, while `rtt_factor: 1.0` left it break-even by design.
///
/// What made it survive is that it was invisible from a test: `bevy_ensemble_loopback` published
/// `PeerRtt` itself, resampling the link's jitter every frame, so the derived estimate worked
/// perfectly in the harness and only failed on a real transport, where the ping smooths first.
///
/// Only the transport sees raw samples, so only the transport can measure this. It arrives as
/// [`PeerRttJitter`] and is used as given.
fn update_estimates(state: &mut AdaptiveBufferState, raw_rtt: f32, jitter: f32) {
    state.jitter = jitter;
    state.ema_rtt = Some(match state.ema_rtt {
        None => raw_rtt,
        Some(prev) => RTT_ALPHA * raw_rtt + (1.0 - RTT_ALPHA) * prev,
    });
}

/// Buffer size (in ticks) the current estimates call for.
fn target_buffer(state: &AdaptiveBufferState, tuning: &AdaptiveBufferTuning) -> Option<u64> {
    let ema_rtt = state.ema_rtt?;
    // Base on the full RTT: a locally-issued action must reach the host and the host's
    // authoritative echo must reach the other peers before the tick runs — round-trip, plus
    // jitter headroom.
    //
    // `rtt_factor` of 1.0 makes this exactly break-even, which is why a session sits fractionally
    // under full tick rate on a steady link and dips below it whenever the link is not steady.
    let latency = tuning.rtt_factor * ema_rtt + JITTER_SAFETY * state.jitter;
    let ticks = (latency / SECONDS_PER_TICK).ceil() as u64 + tuning.extra_ticks;
    Some(ticks.clamp(MIN_BUFFER, tuning.max_buffer))
}

/// Move `buffer` toward `target`: up at once, down a tick at a time.
///
/// The asymmetry is deliberate and is about cost rather than safety. Too small a buffer stalls the
/// session on every tick until it recovers, so growth takes the target immediately. Too large a
/// buffer only costs input latency, and a connection that looks better for a moment usually is
/// not, so shrinking waits out [`SHRINK_COOLDOWN_FRAMES`] and then gives back one tick.
fn apply_target(buffer: &mut u64, target: u64, state: &mut AdaptiveBufferState) {
    use std::cmp::Ordering;

    match target.cmp(buffer) {
        Ordering::Greater => {
            *buffer = target;
            state.frames_since_shrink = state.frames_since_shrink.saturating_add(1);
        }
        Ordering::Less => {
            if state.frames_since_shrink >= SHRINK_COOLDOWN_FRAMES {
                *buffer -= 1;
                state.frames_since_shrink = 0;
            } else {
                state.frames_since_shrink = state.frames_since_shrink.saturating_add(1);
            }
        }
        Ordering::Equal => {
            state.frames_since_shrink = state.frames_since_shrink.saturating_add(1);
        }
    }
}

fn adapt_tick_buffer(
    mut config: ResMut<LockstepConfig>,
    mut state: ResMut<AdaptiveBufferState>,
    tuning: Res<AdaptiveBufferTuning>,
    host_lobby: Query<(), (With<Lobby>, With<Host>)>,
    client_lobby_rtt: Query<(Ref<PeerRtt>, Option<&PeerRttJitter>), (With<Lobby>, Without<Host>)>,
    client_peers: Query<(Ref<PeerRtt>, Option<&PeerRttJitter>), With<LobbyClient>>,
) {
    let is_host = !host_lobby.is_empty();

    // `Ref` rather than `&` so `is_changed` can say whether this is a *new* ping or the same one
    // being read again on the next frame — see `update_estimates`.
    //
    // A backend that publishes no `PeerRttJitter` gets no jitter headroom rather than a guess.
    // That is the honest reading of "this transport does not measure spread", and it is what every
    // backend effectively had before it was measured at all.
    let sample = if is_host {
        // The host must keep up with its slowest peer — and cover its *least steady* one, which
        // need not be the same peer. Taking the worst of each independently is deliberate: a
        // buffer sized for the slow link and the steady link's spread stalls on the jittery one.
        client_peers
            .iter()
            .fold(None::<(f64, f64, bool)>, |worst, (rtt, jitter)| {
                let jitter = jitter.map_or(0.0, |jitter| jitter.0);
                let fresh = rtt.is_changed();
                Some(match worst {
                    None => (rtt.0, jitter, fresh),
                    Some((worst_rtt, worst_jitter, was_fresh)) => (
                        worst_rtt.max(rtt.0),
                        worst_jitter.max(jitter),
                        was_fresh || fresh,
                    ),
                })
            })
    } else {
        client_lobby_rtt.iter().next().map(|(rtt, jitter)| {
            (
                rtt.0,
                jitter.map_or(0.0, |jitter| jitter.0),
                rtt.is_changed(),
            )
        })
    };

    let Some((raw_rtt, raw_jitter, is_fresh_sample)) = sample else {
        // Single-player, or no RTT sample yet: leave the configured buffer as-is.
        return;
    };

    if is_fresh_sample {
        update_estimates(&mut state, raw_rtt as f32, raw_jitter as f32);
    }
    let Some(target) = target_buffer(&state, &tuning) else {
        return;
    };

    let buffer = if is_host {
        &mut config.host_tick_buffer
    } else {
        &mut config.client_tick_buffer
    };
    apply_target(buffer, target, &mut state);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tuning() -> AdaptiveBufferTuning {
        AdaptiveBufferTuning::default()
    }

    #[test]
    fn a_perfect_link_still_buffers_the_scheduling_overhead() {
        let mut state = AdaptiveBufferState::default();
        update_estimates(&mut state, 0.0, 0.0);

        assert_eq!(
            target_buffer(&state, &tuning()),
            Some(MIN_BUFFER),
            "a link with no measurable latency still costs a tick each side to schedule and \
             apply, and a buffer that does not cover it runs the simulation at half speed"
        );
    }

    #[test]
    fn the_estimate_converges_on_a_steady_link() {
        let mut state = AdaptiveBufferState::default();
        update_estimates(&mut state, 5.0, 0.0);
        for _ in 0..200 {
            update_estimates(&mut state, 0.010, 0.0);
        }

        let ema_rtt = state.ema_rtt.expect("a sample was integrated");
        assert!(
            (ema_rtt - 0.010).abs() < 0.001,
            "a peer whose latency fell from 5 s to 10 ms is still estimated at {ema_rtt} s, so \
             its buffer stays pinned at the maximum for the rest of the session"
        );
    }

    #[test]
    fn the_buffer_grows_at_once_and_shrinks_on_a_cooldown() {
        let mut state = AdaptiveBufferState::default();
        let mut buffer = 6;

        apply_target(&mut buffer, 40, &mut state);
        assert_eq!(buffer, 40, "growth takes the target immediately");

        apply_target(&mut buffer, 10, &mut state);
        assert_eq!(buffer, 40, "the first shrink waits out the cooldown");

        for _ in 0..SHRINK_COOLDOWN_FRAMES {
            apply_target(&mut buffer, 10, &mut state);
        }
        assert_eq!(buffer, 39, "and then gives back exactly one tick");
    }

    #[test]
    fn jitter_buys_headroom_over_the_mean() {
        let steady = {
            let mut state = AdaptiveBufferState::default();
            for _ in 0..200 {
                update_estimates(&mut state, 0.100, 0.0);
            }
            target_buffer(&state, &tuning()).expect("a sample was integrated")
        };
        let jittery = {
            let mut state = AdaptiveBufferState::default();
            for _ in 0..200 {
                update_estimates(&mut state, 0.100, 0.040);
            }
            target_buffer(&state, &tuning()).expect("a sample was integrated")
        };

        assert!(
            jittery > steady,
            "two links averaging 100 ms sized the same buffer ({jittery} vs {steady}) — the \
             point of the jitter term is that the unsteady one needs more"
        );
    }

    #[test]
    fn a_transport_that_reports_no_spread_gets_no_headroom_rather_than_a_guess() {
        // The failure this replaces did not look like this. It reported a *number*, derived from
        // an already-smoothed series, that was near zero on links with real spread — so it read
        // as "measured, and small" when it meant "not measured". Absence is now absence.
        let mut state = AdaptiveBufferState::default();
        for _ in 0..200 {
            update_estimates(&mut state, 0.100, 0.0);
        }
        assert_eq!(state.jitter, 0.0);
    }
}
