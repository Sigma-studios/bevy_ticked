//! What a session owns, registered once, so that no door has to remember it.
//!
//! # Why a registry
//!
//! A session used to be undone by hand. Five functions across three crates — the host's door in,
//! the client's door in, the door out, the end of a ticked session and a departed peer's cleanup —
//! each listed the resources it put back, about sixty-five statements between them, and each list
//! was a little different from the others. The code's own comments record seven resources that
//! one door or another forgot and that were added after somebody lost an evening to them; the
//! last was a client's lead controller, whose "settling until tick N" outlived the session that
//! set it and froze the next one's lead for as long as the old clock had run. Every one of them
//! was the same bug: a list that has to be kept in step with the rest of the code by whoever adds
//! the next resource, who has no reason to know the list exists.
//!
//! So a resource says what it belongs to where it is declared, and the doors ask the registry.
//! [`SessionAppExt::init_session_resource`] is `init_resource` plus "and put it back when the
//! session ends"; the doors call [`reset_session_state`] and [`end_session_state`], which reset
//! whatever has been registered, and nothing else needs to change when somebody adds the next one.
//!
//! # Two lifetimes, because a host keeps its world
//!
//! [`SessionScope::Role`] is the link and the clock: what a host knows about its clients, what a
//! client knows about its host, the tick-keyed bookkeeping. It goes at every door — leaving,
//! joining, hosting, a host change, a solo player's world handed to a lobby — because none of it
//! means anything to the next role, and it goes on the way *in* as well as out, so nothing a peer
//! did while it was in a menu reaches the session it enters.
//!
//! [`SessionScope::Session`] is what a *game* means by a session: the visit, from the moment a peer
//! starts playing alone or opens or joins a lobby until it is back where it started. The code it
//! joined by, the map it built, the match it asked for. It goes only when the peer leaves, and not
//! when it changes role inside the visit: a solo player who opens a lobby keeps the arena it is
//! standing in, and a resource that was reset there would rebuild it underneath the players.
//!
//! # Beside the ticked resources, not instead of them
//!
//! [`TickedResourceRegistry`](crate::resource_registry::TickedResourceRegistry) registers
//! resources too, and its `reset_all` puts them back to their defaults. Those are *the world*:
//! simulated state that is captured, rolled back and replicated with the entities, and handed over
//! with them. They go when the world goes — on leaving and on joining — and a host keeps them with
//! its entities. Folding them in here would give every one of them a scope that is already decided
//! by what they are, so they stay where they are, and the doors dispose of the world with them.
//!
//! # Per peer
//!
//! A host holds things per client — its inputs, its margin, its delta baselines — and a client
//! that leaves a running session must take them with it, or a rejoin under the same uuid inherits
//! its own ghost. [`PerPeer`] is that, and [`forget_peer`] runs every registered one.

use core::any::{TypeId, type_name};
use core::fmt::Debug;
use std::sync::Arc;

use bevy::ecs::component::Mutable;
use bevy::ecs::world::DeferredWorld;
use bevy::prelude::*;

use crate::events::TickedEventRegistry;
use crate::registry::TickedComponentRegistry;
use crate::tick::{CurrentTick, TickHoldReason, TickHolds};

/// How long a registered resource lives. See the module note.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SessionScope {
    /// Reset at every door: whenever this peer starts or stops being a host, a client or a solo
    /// player, including a host change and a solo world handed to a lobby. The link and the clock.
    Role,
    /// Reset only when this peer leaves: back to where it was before it played alone, hosted or
    /// joined. A game's `reset_on_leave`.
    Session,
}

/// A resource that is put back when its session ends.
///
/// `reset` defaults to `*self = Self::default()`. Override it to keep what is configuration rather
/// than session — a history's capacity, a tuning a game inserted at startup:
///
/// ```ignore
/// impl SessionReset for AuthoritativeHistory {
///     fn reset(&mut self) {
///         self.clear(); // keeps `max_ticks`
///     }
/// }
/// ```
///
/// `Debug` is required so that a test can say *which* resource outlived a session, and so that
/// [`SessionResources::fingerprints`] can compare a reused app with a fresh one without asking every
/// resource to be `PartialEq`.
pub trait SessionReset: Resource<Mutability = Mutable> + Default + Debug {
    fn reset(&mut self) {
        *self = Self::default();
    }
}

/// A resource that holds something per remote peer, and forgets a peer that left.
pub trait PerPeer: Resource<Mutability = Mutable> {
    fn forget(&mut self, peer: u128);
}

#[derive(Clone)]
struct Entry {
    name: &'static str,
    type_id: TypeId,
    scope: SessionScope,
    reset: fn(&mut World),
    fingerprint: fn(&World) -> Option<String>,
    forget: Option<fn(&mut DeferredWorld, u128)>,
}

/// Every resource registered as belonging to a session. See the module note.
///
/// A resource, so that the doors — which live in other crates and run as exclusive systems — can
/// read it from the world they are resetting. Cloning is cheap: the entries are shared.
#[derive(Resource, Clone, Default)]
pub struct SessionResources {
    entries: Arc<Vec<Entry>>,
}

impl SessionResources {
    fn register(&mut self, entry: Entry) {
        if let Some(existing) = self.entries.iter().find(|e| e.type_id == entry.type_id) {
            // Both role plugins of a listen server register the same resources; the second
            // registration is the first one again. A *different* scope is two crates disagreeing
            // about what the resource is, which is worth a panic at build time.
            assert_eq!(
                existing.scope, entry.scope,
                "`{}` was registered as a session resource twice, with different scopes",
                entry.name
            );
            if entry.forget.is_some() && existing.forget.is_none() {
                let entries = Arc::make_mut(&mut self.entries);
                if let Some(existing) = entries.iter_mut().find(|e| e.type_id == entry.type_id) {
                    existing.forget = entry.forget;
                }
            }
            return;
        }
        Arc::make_mut(&mut self.entries).push(entry);
    }

    /// Reset every resource registered with `scope`. Absent resources stay absent.
    pub fn reset(&self, world: &mut World, scope: SessionScope) {
        for entry in self.entries.iter().filter(|entry| entry.scope == scope) {
            (entry.reset)(world);
        }
    }

    /// Forget `peer` in every [`PerPeer`] resource.
    ///
    /// Takes a [`DeferredWorld`] so that an observer can call it and have the peer forgotten by
    /// the time the trigger returns, rather than a command flush later.
    pub fn forget_peer(&self, world: &mut DeferredWorld, peer: u128) {
        for forget in self.entries.iter().filter_map(|entry| entry.forget) {
            forget(world, peer);
        }
    }

    /// The type name of every registered resource, with its scope, in registration order.
    pub fn names(&self) -> impl Iterator<Item = (&'static str, SessionScope)> + '_ {
        self.entries.iter().map(|entry| (entry.name, entry.scope))
    }

    /// Every registered resource as it stands, printed: `None` for one that is absent.
    ///
    /// For tests. Two apps whose fingerprints agree hold the same session state, which is how
    /// "a reused app is a fresh app" is checked without a list of resources that has to be kept
    /// in step with this registry — the list *is* this registry.
    pub fn fingerprints(&self, world: &World) -> Vec<(&'static str, Option<String>)> {
        self.entries
            .iter()
            .map(|entry| (entry.name, (entry.fingerprint)(world)))
            .collect()
    }
}

fn reset_resource<R: SessionReset>(world: &mut World) {
    if let Some(mut resource) = world.get_resource_mut::<R>() {
        resource.reset();
    }
}

fn remove_resource<R: Resource>(world: &mut World) {
    world.remove_resource::<R>();
}

fn fingerprint_resource<R: Resource + Debug>(world: &World) -> Option<String> {
    world
        .get_resource::<R>()
        .map(|resource| format!("{resource:?}"))
}

fn fingerprint_presence<R: Resource>(world: &World) -> Option<String> {
    world.contains_resource::<R>().then(|| "present".to_owned())
}

fn forget_in<R: PerPeer>(world: &mut DeferredWorld, peer: u128) {
    if let Some(mut resource) = world.get_resource_mut::<R>() {
        resource.forget(peer);
    }
}

/// Registering session state on an [`App`].
pub trait SessionAppExt {
    /// `init_resource`, and reset it when this peer leaves its session ([`SessionScope::Session`]).
    ///
    /// The one a game wants: whatever its `reset_on_leave` system used to put back.
    ///
    /// ```ignore
    /// #[derive(Resource, Default, Debug)]
    /// struct JoinedCode(Option<String>);
    /// impl SessionReset for JoinedCode {}
    ///
    /// app.init_session_resource::<JoinedCode>();
    /// ```
    fn init_session_resource<R: SessionReset>(&mut self) -> &mut Self;

    /// `init_resource`, and reset it at the end of `scope`.
    fn init_session_resource_scoped<R: SessionReset>(&mut self, scope: SessionScope) -> &mut Self;

    /// Reset `R` at the end of `scope` if it is present, without inserting it.
    ///
    /// For a resource that exists only in some configurations and whose absence means something:
    /// the tick-rate dilation only exists under a steerable clock, and a client checks for it.
    fn register_session_resource<R: SessionReset>(&mut self, scope: SessionScope) -> &mut Self;

    /// `init_resource`, reset at every door ([`SessionScope::Role`]), and forget a departed peer
    /// in it ([`forget_peer`]).
    fn init_per_peer_resource<R: SessionReset + PerPeer>(&mut self) -> &mut Self;

    /// A marker resource — present or absent, nothing inside — removed at the end of `scope`.
    ///
    /// Not inserted: absence is the state a session starts in.
    fn remove_at_session_end<R: Resource>(&mut self, scope: SessionScope) -> &mut Self;
}

impl SessionAppExt for App {
    fn init_session_resource<R: SessionReset>(&mut self) -> &mut Self {
        self.init_session_resource_scoped::<R>(SessionScope::Session)
    }

    fn init_session_resource_scoped<R: SessionReset>(&mut self, scope: SessionScope) -> &mut Self {
        self.init_resource::<R>();
        self.register_session_resource::<R>(scope)
    }

    fn register_session_resource<R: SessionReset>(&mut self, scope: SessionScope) -> &mut Self {
        register(self.world_mut(), entry_for::<R>(scope));
        self
    }

    fn init_per_peer_resource<R: SessionReset + PerPeer>(&mut self) -> &mut Self {
        self.init_resource::<R>();
        let mut entry = entry_for::<R>(SessionScope::Role);
        entry.forget = Some(forget_in::<R>);
        register(self.world_mut(), entry);
        self
    }

    fn remove_at_session_end<R: Resource>(&mut self, scope: SessionScope) -> &mut Self {
        register(
            self.world_mut(),
            Entry {
                name: type_name::<R>(),
                type_id: TypeId::of::<R>(),
                scope,
                reset: remove_resource::<R>,
                fingerprint: fingerprint_presence::<R>,
                forget: None,
            },
        );
        self
    }
}

fn entry_for<R: SessionReset>(scope: SessionScope) -> Entry {
    Entry {
        name: type_name::<R>(),
        type_id: TypeId::of::<R>(),
        scope,
        reset: reset_resource::<R>,
        fingerprint: fingerprint_resource::<R>,
        forget: None,
    }
}

fn register(world: &mut World, entry: Entry) {
    world
        .get_resource_or_insert_with(SessionResources::default)
        .register(entry);
}

/// Put back everything a role owns, and restart the clock. Every door calls it, in and out.
///
/// - Every [`SessionScope::Role`] resource is reset.
/// - The clock is at zero, and nothing remembers a tick of the old one: the component and resource
///   histories, the entity lifetimes, the event logs. The next session's tick 40 is not the last
///   one's, and a history, a log or a watermark that said otherwise would make it so.
/// - Tick 0 may be captured again: the entry door calls
///   [`capture_initial_state`](crate::capture_initial_state) once it has decided what world
///   stands, so a host that keeps its solo world can rewind to the start of its session like a
///   fresh one.
/// - The clock is released from every hold that belonged to a session: waiting for a first
///   snapshot, a soft hold, a session pause, a replay. A game's own holds — its pause menu — are
///   the game's, and survive.
///
/// The world itself is not touched: whether it stays (a host, a solo player) or goes (a client,
/// a leave) is the door's decision, not a property of the state.
pub fn reset_session_state(world: &mut World) {
    if let Some(resources) = world.get_resource::<SessionResources>().cloned() {
        resources.reset(world, SessionScope::Role);
    }
    world.insert_resource(CurrentTick(crate::tick_types::Tick::ZERO));
    if let Some(registry) = world.get_resource::<TickedComponentRegistry>().cloned() {
        registry.clear_all(world);
    }
    TickedEventRegistry::clear_all(world);
    if let Some(mut holds) = world.get_resource_mut::<TickHolds>() {
        for reason in [
            TickHoldReason::AwaitingSync,
            TickHoldReason::SoftHold,
            TickHoldReason::SessionPause,
            TickHoldReason::Replaying,
        ] {
            holds.release(reason);
        }
    }
}

/// Put back everything registered for the whole session ([`SessionScope::Session`]). Only the
/// leave calls it; see the module note for why a role change does not.
pub fn end_session_state(world: &mut World) {
    if let Some(resources) = world.get_resource::<SessionResources>().cloned() {
        resources.reset(world, SessionScope::Session);
    }
}

/// Forget `peer` in every [`PerPeer`] resource: a client left a session that carries on.
pub fn forget_peer(world: &mut DeferredWorld, peer: u128) {
    if let Some(resources) = world.get_resource::<SessionResources>().cloned() {
        resources.forget_peer(world, peer);
    }
}

impl SessionReset for crate::time::TickRateDilation {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[derive(Resource, Default, Debug, PartialEq)]
    struct Code(Option<&'static str>);
    impl SessionReset for Code {}

    #[derive(Resource, Default, Debug, PartialEq)]
    struct Margins(BTreeMap<u128, i64>);
    impl SessionReset for Margins {}
    impl PerPeer for Margins {
        fn forget(&mut self, peer: u128) {
            self.0.remove(&peer);
        }
    }

    /// Configuration kept across the reset, the reason `reset` is overridable.
    #[derive(Resource, Debug, PartialEq)]
    struct History {
        kept: Vec<u64>,
        capacity: usize,
    }
    impl Default for History {
        fn default() -> Self {
            Self {
                kept: Vec::new(),
                capacity: 64,
            }
        }
    }
    impl SessionReset for History {
        fn reset(&mut self) {
            self.kept.clear();
        }
    }

    #[derive(Resource)]
    struct Verified;

    fn app() -> App {
        let mut app = App::new();
        app.init_session_resource::<Code>()
            .init_per_peer_resource::<Margins>()
            .init_session_resource_scoped::<History>(SessionScope::Role)
            .remove_at_session_end::<Verified>(SessionScope::Role);
        app
    }

    #[test]
    fn a_role_door_resets_the_role_and_leaves_the_session() {
        let mut app = app();
        let world = app.world_mut();
        world.resource_mut::<Code>().0 = Some("ABCD");
        world.resource_mut::<Margins>().0.insert(3, 2);
        *world.resource_mut::<History>() = History {
            kept: vec![1, 2],
            capacity: 256,
        };
        world.insert_resource(Verified);

        reset_session_state(world);
        assert_eq!(
            world.resource::<Code>().0,
            Some("ABCD"),
            "the visit goes on"
        );
        assert!(world.resource::<Margins>().0.is_empty());
        assert_eq!(
            *world.resource::<History>(),
            History {
                kept: Vec::new(),
                capacity: 256
            },
            "an overridden reset keeps what is configuration"
        );
        assert!(!world.contains_resource::<Verified>());

        end_session_state(world);
        assert_eq!(world.resource::<Code>().0, None, "and the leave ends it");
    }

    #[test]
    fn a_departed_peer_is_forgotten_and_nobody_else() {
        let mut app = app();
        let world = app.world_mut();
        world.resource_mut::<Margins>().0.extend([(3, 2), (4, 1)]);
        forget_peer(&mut world.into(), 3);
        assert_eq!(world.resource::<Margins>().0, BTreeMap::from([(4, 1)]));
    }

    #[test]
    fn registering_twice_is_registering_once() {
        let mut app = app();
        app.init_session_resource::<Code>();
        let names: Vec<_> = app.world().resource::<SessionResources>().names().collect();
        assert_eq!(names.len(), 4);
    }

    #[test]
    #[should_panic(expected = "different scopes")]
    fn two_scopes_for_one_resource_is_refused() {
        let mut app = app();
        app.init_session_resource_scoped::<Code>(SessionScope::Role);
    }
}
