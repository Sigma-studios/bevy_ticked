//! The capture path allocates nothing once it is warm.
//!
//! `capture_component` used to build a fresh `HashMap` per registered type per tick and the
//! prune dropped one; sixty-four times a second, for every type, on every peer, that was the
//! steadiest allocator traffic in a game that had none inside its tick. Now the per-tick maps
//! are recycled and the capture query is cached, and this test holds the path to zero bytes.
//!
//! Alone in its binary on purpose: the allocator is global, and a second test running on
//! another thread while the count is open would be counted too.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ticked::prelude::*;
use bevy_ticked::registry::TickedComponentRegistry;
use bevy_ticked::tracked_entity::TickTrackedEntity;

/// `System`, with every allocation counted while [`COUNTING`] is set.
struct Counting;

static COUNTING: AtomicBool = AtomicBool::new(false);
static BYTES: AtomicUsize = AtomicUsize::new(0);
static CALLS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            BYTES.fetch_add(layout.size(), Ordering::Relaxed);
            CALLS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            BYTES.fetch_add(new_size, Ordering::Relaxed);
            CALLS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn start_counting() {
    BYTES.store(0, Ordering::Relaxed);
    CALLS.store(0, Ordering::Relaxed);
    COUNTING.store(true, Ordering::SeqCst);
}

fn stop_counting() -> (usize, usize) {
    COUNTING.store(false, Ordering::SeqCst);
    (BYTES.load(Ordering::Relaxed), CALLS.load(Ordering::Relaxed))
}

/// `Copy`, so cloning it into history is not itself an allocation; a component that owns a
/// `Vec` would allocate on clone, and that is the component's business, not the capture's.
#[derive(Component, Clone, Copy, Debug, PartialEq)]
struct Pos(i32);

#[derive(Component, Clone, Copy, Debug, PartialEq)]
struct Vel(i32);

const ENTITIES: u64 = 50;
const WARMUP_TICKS: usize = 100;
const MEASURED_TICKS: u64 = 10;
/// Small enough that the prune runs on every warm-up tick, so the measured ticks run in the
/// steady state a long session is in: one map taken per capture, one handed back per prune.
const WINDOW: u64 = 32;

/// Measures `capture_all` and `prune_all_before` called directly, the way `advance_one_tick`
/// calls them, rather than a whole `app.update()`: Bevy's own frame — schedule executors,
/// command queues, change-detection bookkeeping — allocates on its own account, and that is
/// not what this test is about. What it is about is that this crate's part of the tick adds
/// nothing to it.
#[test]
fn capturing_a_tick_after_warmup_allocates_nothing() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(TickedPlugin {
            source: TickSource::Manual,
            ..default()
        })
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_micros(
            15_625,
        )))
        .insert_resource(HistoryBufferTicks(WINDOW))
        .register_ticked_component::<Pos>()
        .register_ticked_component::<Vel>();
    for i in 0..ENTITIES {
        app.world_mut()
            .spawn((TickTrackedEntity(i + 1), Pos(i as i32), Vel(1)));
    }

    for _ in 0..WARMUP_TICKS {
        app.world_mut().write_message(StepForward);
        app.update();
    }
    let warm_tick = app.world().resource::<CurrentTick>().0;
    assert!(
        warm_tick as usize >= WARMUP_TICKS,
        "the warm-up ran {warm_tick} ticks"
    );

    let registry = app.world().resource::<TickedComponentRegistry>().clone();
    let world = app.world_mut();

    start_counting();
    for tick in warm_tick + 1..=warm_tick + MEASURED_TICKS {
        registry.capture_all(world, tick);
        registry.prune_all_before(world, tick.saturating_sub(WINDOW));
    }
    let (bytes, calls) = stop_counting();

    assert_eq!(
        bytes, 0,
        "{MEASURED_TICKS} warm ticks of capture allocated {bytes} bytes in {calls} calls; \
         the per-tick maps or the capture query are not being reused"
    );
    assert!(
        registry.has_tick_captured(world, warm_tick + MEASURED_TICKS),
        "and the captures actually happened"
    );
}
