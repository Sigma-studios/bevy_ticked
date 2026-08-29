use crate::{
    ActionTracker, ClientScheduledActions, ClientSnapshotState, JoinSnapshot, LocalPendingActions,
    LockstepAction, LockstepConfig,
};
use bevy::prelude::*;
use bevy_ensemble::{Host, Lobby, LobbyMessage, LocalMultiplayerPlayerId, ReceivedEnsembleMessage};
use bevy_ticked::tick::{CurrentTick, TicksPaused};

/// Record a player's actions for a tick, joining them to anything already recorded.
///
/// Merges rather than replaces. Two batches can legitimately land on the same tick — a buffer that
/// shrinks makes two consecutive flushes target one tick — and replacing meant the first batch was
/// silently dropped, taking a player's building placement or movement change with it. Nothing
/// logged it and nothing could detect it.
///
/// For the ordinary case where the key is absent, `or_default()` plus `extend` on an empty `Vec`
/// is exactly what `insert` did.
pub fn insert_actions_into_tracker<A>(
    tracker: &mut ActionTracker<A>,
    tick: u64,
    player_uuid: u128,
    actions: Vec<A>,
) {
    tracker
        .ticks
        .entry(tick)
        .or_default()
        .entry(player_uuid)
        .or_default()
        .extend(actions);
}

/// The most recent tick this peer scheduled actions for.
///
/// `None` before the first flush, and reset whenever the tracker is rebuilt from a join snapshot —
/// there is no sequence to stay contiguous with at that point.
#[derive(Resource, Default, Debug)]
pub struct LastScheduledTick(pub Option<u64>);

/// Flush pending local actions into the tracker (host) or send to host (client).
///
/// Runs in `FixedUpdate::TickedSystems::PreTick`. Each invocation corresponds to one
/// upcoming tick advancement. Actions are scheduled `buffer` ticks ahead.
///
/// # Why this emits a range rather than one tick
///
/// The host requires an entry from every established participant for every tick, and has no
/// gap-fill and no timeout: a tick nobody scheduled for stops the session dead, for ever.
///
/// The scheduled tick is `current_tick + 1 + buffer`, and `buffer` is not constant — it is a
/// mutable resource precisely so consumers can size it from measured latency. The tick advances by
/// one between flushes, so **any** increase in `buffer` skips a tick:
///
/// ```text
/// flush at tick C, buffer B      -> schedules C + 1 + B
/// flush at tick C+1, buffer B+1  -> schedules C + 3 + B    (C + 2 + B never scheduled)
/// ```
///
/// It is worse while paused, because a paused peer does not flush but its buffer keeps adapting,
/// so the hole that opens on resume is as wide as the buffer moved. Rising latency causes both the
/// stall and the growth, so the two feed each other — which is why the symptom is a session that
/// freezes a second or two after somebody's connection degrades and never recovers.
///
/// So every tick from the last scheduled one onwards gets a batch: the real actions on the newest,
/// and empty batches to cover the gap. Empty is correct rather than merely convenient — those
/// ticks were never going to carry input, and what the host is waiting for is not the content but
/// the statement that this participant has nothing more to say about that tick.
pub fn flush_pending_actions<A: LockstepAction, S: JoinSnapshot>(
    ticks_paused: Option<Res<TicksPaused>>,
    pending_actions: Option<ResMut<LocalPendingActions<A>>>,
    snapshot_state: Option<Res<ClientSnapshotState<S>>>,
    config: Res<LockstepConfig>,
    current_tick: Res<CurrentTick>,
    mut tracker: ResMut<ActionTracker<A>>,
    mut last_scheduled: ResMut<LastScheduledTick>,
    local_player_id: Option<Res<LocalMultiplayerPlayerId>>,
    host_lobbies: Query<(), (With<Lobby>, With<Host>)>,
    client_lobby: Option<Single<Entity, (With<Lobby>, Without<Host>)>>,
    mut commands: Commands,
) {
    if ticks_paused.is_some() {
        return;
    }

    let Some(mut pending_actions) = pending_actions else {
        return;
    };

    let is_host = !host_lobbies.is_empty();
    let local_player_uuid = local_player_id.as_ref().map(|p| p.0).unwrap_or(0);
    let client_snapshot_ready = snapshot_state.as_ref().is_none_or(|state| state.ready);

    if client_lobby.is_some() && !client_snapshot_ready {
        return;
    }

    let actions = std::mem::take(&mut pending_actions.0);
    // The next tick that will run is current_tick.0 + 1
    let next_tick = current_tick.0 + 1;

    let buffer = if is_host {
        config.host_tick_buffer
    } else {
        config.client_tick_buffer
    };

    if client_lobby.is_some() || is_host {
        let scheduled_tick = next_tick + buffer;
        // Everything between the last flush and this one, so the sequence has no holes. Usually
        // empty: in the steady state `scheduled_tick` is exactly one past the last.
        let filler = match last_scheduled.0 {
            Some(last) if scheduled_tick > last + 1 => (last + 1)..scheduled_tick,
            _ => scheduled_tick..scheduled_tick,
        };
        last_scheduled.0 = Some(scheduled_tick.max(last_scheduled.0.unwrap_or(0)));

        if let Some(ref client_lobby) = client_lobby {
            // `new_no_delay`, here and for the batch below: the host blocks until this arrives,
            // so it is the definition of a message something is waiting on. A coalescing send
            // holds it for a few milliseconds hoping to pack it with the next one — which is a
            // whole tick away and will never come in time — and the host spends that wait
            // paused. See `bevy_ensemble::SendMode`.
            for tick in filler {
                let message = ClientScheduledActions::<A> {
                    tick,
                    actions: Vec::new(),
                };
                commands
                    .entity(**client_lobby)
                    .trigger(move |entity| LobbyMessage::new_no_delay(entity, message));
            }
            let message = ClientScheduledActions {
                tick: scheduled_tick,
                actions,
            };
            commands
                .entity(**client_lobby)
                .trigger(move |entity| LobbyMessage::new_no_delay(entity, message));
        } else {
            for tick in filler {
                insert_actions_into_tracker(&mut tracker, tick, local_player_uuid, Vec::new());
            }
            insert_actions_into_tracker(&mut tracker, scheduled_tick, local_player_uuid, actions);
        }
        return;
    }

    // Single-player / no lobby: insert directly at the next tick
    if !actions.is_empty() {
        insert_actions_into_tracker(&mut tracker, next_tick, local_player_uuid, actions);
    }
}

pub fn receive_client_actions<A: LockstepAction>(
    mut messages: MessageReader<ReceivedEnsembleMessage<ClientScheduledActions<A>>>,
    mut tracker: ResMut<ActionTracker<A>>,
    host_lobbies: Query<(), (With<Lobby>, With<Host>)>,
) {
    if host_lobbies.is_empty() {
        return;
    }

    for message in messages.read() {
        let Some(sender) = message.sender else {
            continue;
        };
        insert_actions_into_tracker(
            &mut tracker,
            message.message.tick,
            sender,
            message.message.actions.clone(),
        );
    }
}
