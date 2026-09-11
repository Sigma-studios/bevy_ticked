use crate::{
    ActionTracker, ClientScheduledActions, ClientSnapshotState, JoinSnapshot, LocalPendingActions,
    LockstepAction, LockstepConfig, LockstepLobbyParticipant,
};
use bevy::prelude::*;
use bevy_ensemble::{
    Host, Lobby, LobbyMessage, LobbyParticipant, LobbyParticipantOf, LocalMultiplayerPlayerId,
    ReceivedEnsembleMessage,
};
use bevy_ticked::tick::{CurrentTick, TickHolds};

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
/// `None` before the first flush and while a join snapshot is in flight; a client that has
/// applied one restarts it at the snapshot's tick, so its first flush fills forward from there.
#[derive(Resource, Default, Debug)]
pub struct LastScheduledTick(pub Option<u64>);

/// Flush pending local actions into the tracker (host, solo) or send them to the host (client).
///
/// Runs in `TickedLoop::PreTick`. Each invocation corresponds to one upcoming tick advancement.
///
/// # The host's own actions go into the tick about to run
///
/// A client schedules `client_tick_buffer` ticks ahead because its actions have to cross the
/// link and be ruled on before the tick they name is simulated. The host's do not cross anything:
/// it is the one ruling. So they go straight into `next_tick`, the tick is simulated with them,
/// and the authoritative broadcast that follows carries them to every client — with no input lag
/// and nothing for the host to wait on.
///
/// The host used to schedule `host_tick_buffer` ahead as well, as though it were a client of
/// itself, and its own entry then gated its own advance: with a buffer of zero the pause check
/// looked for the host's entry for `next_tick` *before* this flush had inserted it, held, and —
/// because this flush does not run while held — never inserted it. `host_tick_buffer` is now
/// what it always effectively was: the grace window the host gives a client's actions.
///
/// # Why a client emits a range rather than one tick
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
///
/// # No identity, no actions
///
/// A peer without a `LocalMultiplayerPlayerId` has nobody to file its actions under. They used to
/// go under uuid 0, which is a real-looking player that no roster contains: the host recorded
/// them, broadcast them, and every client applied input from a player that did not exist. They
/// are dropped instead, with a warning the first time.
pub fn flush_pending_actions<A: LockstepAction, S: JoinSnapshot>(
    holds: Res<TickHolds>,
    pending_actions: Option<ResMut<LocalPendingActions<A>>>,
    snapshot_state: Option<Res<ClientSnapshotState<S>>>,
    config: Res<LockstepConfig>,
    current_tick: Res<CurrentTick>,
    mut tracker: ResMut<ActionTracker<A>>,
    mut last_scheduled: ResMut<LastScheduledTick>,
    local_player_id: Option<Res<LocalMultiplayerPlayerId>>,
    client_lobby: Option<Single<Entity, (With<Lobby>, Without<Host>)>>,
    mut commands: Commands,
) {
    if holds.is_held() {
        return;
    }

    let Some(mut pending_actions) = pending_actions else {
        return;
    };

    let client_snapshot_ready = snapshot_state.as_ref().is_none_or(|state| state.ready);
    if client_lobby.is_some() && !client_snapshot_ready {
        return;
    }

    let actions = std::mem::take(&mut pending_actions.0);
    let Some(local_player_uuid) = local_player_id.as_ref().map(|player| player.0) else {
        if !actions.is_empty() {
            warn_once!(
                "dropping local lockstep actions: this peer has no LocalMultiplayerPlayerId, so \
                 there is no player to file them under"
            );
        }
        return;
    };

    // The next tick that will run is current_tick.0 + 1
    let next_tick = current_tick.0 + 1;

    let Some(client_lobby) = client_lobby else {
        // Host or solo: into the tick about to run. Recorded only when there is something to
        // record; an absent entry is an empty one, and the broadcast never waits on the host.
        last_scheduled.0 = Some(next_tick);
        if !actions.is_empty() {
            insert_actions_into_tracker(&mut tracker, next_tick, local_player_uuid, actions);
        }
        return;
    };

    let scheduled_tick = next_tick + config.client_tick_buffer;
    // Everything between the last flush and this one, so the sequence has no holes. Usually
    // empty: in the steady state `scheduled_tick` is exactly one past the last.
    let filler = match last_scheduled.0 {
        Some(last) if scheduled_tick > last + 1 => (last + 1)..scheduled_tick,
        _ => scheduled_tick..scheduled_tick,
    };
    last_scheduled.0 = Some(scheduled_tick.max(last_scheduled.0.unwrap_or(0)));

    // `new_no_delay`, here and for the batch below: the host blocks until this arrives, so it
    // is the definition of a message something is waiting on. A coalescing send holds it for a
    // few milliseconds hoping to pack it with the next one — which is a whole tick away and
    // will never come in time — and the host spends that wait paused. See
    // `bevy_ensemble::SendMode`.
    for tick in filler {
        let message = ClientScheduledActions::<A> {
            tick,
            actions: Vec::new(),
        };
        commands
            .entity(*client_lobby)
            .trigger(move |entity| LobbyMessage::new_no_delay(entity, message));
    }
    let message = ClientScheduledActions {
        tick: scheduled_tick,
        actions,
    };
    commands
        .entity(*client_lobby)
        .trigger(move |entity| LobbyMessage::new_no_delay(entity, message));
}

/// Host: record what a client scheduled, if it is from a participant and for a tick still open.
///
/// Three things are refused, and each refusal used to be a silent desync or a way to grow the
/// host's memory from outside:
///
/// * **A sender that is not a participant.** A connected client that has not loaded is not in
///   anybody's roster, so nothing it schedules can be part of a tick every peer agrees on — and
///   the tracker recorded it anyway, to be broadcast under a uuid no client knew.
/// * **A tick the host has already simulated** (`tick <= current`). Merging it changed the
///   tracker's record of a tick that had already been ruled on and broadcast. The clients had
///   applied the tick without it; the next client to join was caught up *with* it, from the
///   tracker; the two disagreed from then on, silently. It happens routinely and honestly: a
///   joiner's first flush fills from its snapshot tick, and the host is past that by the time
///   the batch lands. Late is late; the tick is done.
/// * **A tick past the horizon** (`tick > current + action_horizon`). Every tick a client names
///   is a tracker entry the host keeps until it simulates that tick. A client naming
///   `u64::MAX` kept one for ever; a client naming a million of them kept a million.
///   [`LockstepConfig::action_horizon`] bounds what any one message can reserve.
pub fn receive_client_actions<A: LockstepAction>(
    mut messages: MessageReader<ReceivedEnsembleMessage<ClientScheduledActions<A>>>,
    mut tracker: ResMut<ActionTracker<A>>,
    current_tick: Res<CurrentTick>,
    config: Res<LockstepConfig>,
    host_lobby: Option<Single<Entity, (With<Lobby>, With<Host>)>>,
    participants: Query<(&LobbyParticipant, &LobbyParticipantOf), With<LockstepLobbyParticipant>>,
    mut margins: ResMut<crate::ArrivalMargins>,
) {
    let Some(host_lobby) = host_lobby else {
        return;
    };

    let horizon = current_tick.0.saturating_add(config.action_horizon);
    for message in messages.read() {
        let Some(sender) = message.sender else {
            continue;
        };
        let tick = message.message.tick;

        if !participants
            .iter()
            .any(|(participant, of)| of.0 == *host_lobby && participant.player_uuid == sender)
        {
            debug!("dropping actions for tick {tick} from {sender}, which is not a participant");
            continue;
        }
        if tick <= current_tick.0 {
            debug!(
                "dropping actions from {sender} for tick {tick}: already simulated (at {})",
                current_tick.0
            );
            continue;
        }
        if tick > horizon {
            debug!(
                "dropping actions from {sender} for tick {tick}: past the horizon ({horizon})"
            );
            continue;
        }
        // How early this batch was: the number the client sizes its buffer from.
        let margin = (tick as i64 - current_tick.0 as i64).clamp(i16::MIN as i64, i16::MAX as i64);
        margins.0.insert(sender, margin as i16);

        insert_actions_into_tracker(
            &mut tracker,
            tick,
            sender,
            message.message.actions.clone(),
        );
    }
}
