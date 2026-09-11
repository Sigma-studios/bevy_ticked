//! Is this still the same game it was? Record a trace, and hold it against a recorded copy.
//!
//! Every other assertion in this crate compares one peer against another. That is the right
//! question for netcode and the wrong one for a refactor: a change that reroutes every bullet
//! keeps all the peers in perfect agreement about the new, different world, and passes the lot.
//! A golden trace compares against **what this world used to do**.
//!
//! # The rule
//!
//! **Compute the reference by a different route than the thing it checks.** A golden file is
//! only evidence if the code that produced it is not the code under test: a trace re-recorded
//! from the current build proves the build agrees with itself. So the file is written once, by
//! hand (`UPDATE_GOLDEN=1`), *reviewed as a diff*, and committed. From then on every run is a
//! claim that nothing changed, checked by something the change could not have rewritten.
//!
//! # When one fails
//!
//! It is telling you the simulation now behaves differently. That is sometimes intended and
//! usually not. The failure names the first tick that differs, which is normally enough to say
//! which. To re-record after an intended change:
//!
//! ```text
//! UPDATE_GOLDEN=1 cargo test --test <the test>
//! ```
//!
//! **Then read the diff to the fixture before committing it.** A re-recorded trace is a claim
//! that the new behaviour is correct, and the diff is the only place that claim is ever checked.
//!
//! # File format
//!
//! A [`HashTrace`] is written as text, one `tick value` per line with the value in hex, so the
//! diff of a re-recording reads as "tick 640 changed" in review. Any other [`Trace`] is written
//! as `postcard` bytes in hex, one line, via the trait's default [`encode`](Trace::encode) and
//! [`decode`](Trace::decode); override them for a type whose diff should be readable.

use bevy::prelude::*;
use bevy_ticked::checksum::WorldHash;
use bevy_ticked::tick::CurrentTick;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::path::Path;

/// Something that can be recorded, written to a file, read back, and compared sample by sample.
pub trait Trace: Serialize + DeserializeOwned {
    /// The first place `self` and `other` disagree, described for a person: which tick, and
    /// what each side has there. `None` when they agree.
    fn first_difference(&self, other: &Self) -> Option<String>;

    /// The file form. Defaults to `postcard` bytes as one line of lowercase hex.
    fn encode(&self) -> String {
        let bytes = postcard::to_allocvec(self).expect("a trace is plain data");
        let mut hex = String::with_capacity(bytes.len() * 2 + 1);
        for byte in bytes {
            hex.push_str(&format!("{byte:02x}"));
        }
        hex.push('\n');
        hex
    }

    /// The inverse of [`encode`](Self::encode). `Err` describes what was wrong with the text.
    fn decode(text: &str) -> Result<Self, String> {
        let bytes = from_hex(text.trim())?;
        postcard::from_bytes(&bytes).map_err(|error| format!("not a valid trace: {error}"))
    }
}

/// `(tick, hash)` samples in tick order — a [`WorldHash`] per sampled tick.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct HashTrace {
    pub samples: Vec<(u64, u64)>,
}

impl Trace for HashTrace {
    /// The first sample whose tick or hash differs, or the point where one trace ends and the
    /// other does not.
    fn first_difference(&self, other: &Self) -> Option<String> {
        for (index, mine) in self.samples.iter().enumerate() {
            match other.samples.get(index) {
                Some(theirs) if theirs == mine => continue,
                Some((their_tick, their_hash)) if their_tick == &mine.0 => {
                    return Some(format!(
                        "tick {}: this trace has {:#018x}, the other has {:#018x}",
                        mine.0, mine.1, their_hash
                    ));
                }
                Some((their_tick, _)) => {
                    return Some(format!(
                        "sample {index}: this trace sampled tick {}, the other sampled tick \
                         {their_tick}; the traces were not recorded on the same schedule",
                        mine.0
                    ));
                }
                None => {
                    return Some(format!(
                        "tick {}: this trace continues ({} samples) where the other ends ({} \
                         samples)",
                        mine.0,
                        self.samples.len(),
                        other.samples.len()
                    ));
                }
            }
        }
        other.samples.get(self.samples.len()).map(|(tick, _)| {
            format!(
                "tick {tick}: the other trace continues ({} samples) where this one ends ({} \
                 samples)",
                other.samples.len(),
                self.samples.len()
            )
        })
    }

    fn encode(&self) -> String {
        let mut text = String::new();
        for (tick, hash) in &self.samples {
            text.push_str(&format!("{tick} {hash:#018x}\n"));
        }
        text
    }

    fn decode(text: &str) -> Result<Self, String> {
        let mut samples = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let (tick, hash) = line
                .split_once(' ')
                .ok_or_else(|| format!("line {}: expected `tick hash`, got `{line}`", index + 1))?;
            let tick: u64 = tick
                .parse()
                .map_err(|error| format!("line {}: bad tick `{tick}`: {error}", index + 1))?;
            let hash = u64::from_str_radix(hash.trim_start_matches("0x"), 16)
                .map_err(|error| format!("line {}: bad hash `{hash}`: {error}", index + 1))?;
            samples.push((tick, hash));
        }
        Ok(Self { samples })
    }
}

/// Run `ticks` frames of `app`, sampling `H` after every `every`th one.
///
/// Each `app.update()` is one frame; a test peer built by this crate runs exactly one tick per
/// frame, which is what makes "tick" the right word here. The sample is labelled with the
/// peer's [`CurrentTick`] rather than the frame count, so a peer that ran two ticks in a frame,
/// or none, is recorded as such — and a trace whose ticks are not `1, 2, 3…` is telling you the
/// peer was not stepping one tick per frame.
///
/// `H::sample` is documented as running inside the tick; here it runs between frames, and sees
/// the state the last tick left. For a determinism trace that is the same thing.
///
/// # Panics
///
/// If `every` is 0.
pub fn record_trace<H: WorldHash>(app: &mut App, ticks: u64, every: u64) -> HashTrace {
    assert!(every > 0, "record_trace: `every` must be at least 1");
    let mut trace = HashTrace::default();
    for frame in 1..=ticks {
        app.update();
        if frame % every != 0 {
            continue;
        }
        let world = app.world_mut();
        let tick = world
            .get_resource::<CurrentTick>()
            .map_or(frame, |current| current.0);
        let hash = H::sample(world).value();
        trace.samples.push((tick, hash));
    }
    trace
}

/// `actual` matches the trace recorded at `path`, or `UPDATE_GOLDEN=1` records it there.
///
/// Panics naming the first difference and the command to re-record with, or — when there is no
/// file yet — the command to record one. Read the module docs before re-recording: the diff to
/// the fixture is the only review the new behaviour gets.
#[track_caller]
pub fn check_golden<T: Trace>(path: &Path, actual: &T) {
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .unwrap_or_else(|error| panic!("cannot create `{}`: {error}", parent.display()));
        }
        std::fs::write(path, actual.encode())
            .unwrap_or_else(|error| panic!("cannot write `{}`: {error}", path.display()));
        eprintln!(
            "re-recorded `{}` — review the diff before committing it",
            path.display()
        );
        return;
    }

    let text = std::fs::read_to_string(path).unwrap_or_else(|error| {
        panic!(
            "no golden trace at `{}` ({error}).\n\
             Record one with:\n    UPDATE_GOLDEN=1 cargo test\n\
             and commit it, having read it.",
            path.display()
        )
    });
    let golden = T::decode(&text).unwrap_or_else(|error| {
        panic!(
            "the golden trace at `{}` could not be read: {error}\n\
             Re-record it with:\n    UPDATE_GOLDEN=1 cargo test",
            path.display()
        )
    });

    if let Some(difference) = actual.first_difference(&golden) {
        panic!(
            "the simulation no longer does what `{}` recorded.\n\n  {difference}\n\n\
             If that change is intended, re-record with:\n    UPDATE_GOLDEN=1 cargo test\n\
             and read the diff to the fixture before committing it.",
            path.display()
        );
    }
}

fn from_hex(text: &str) -> Result<Vec<u8>, String> {
    if !text.len().is_multiple_of(2) {
        return Err("odd number of hex digits".to_string());
    }
    (0..text.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&text[index..index + 2], 16)
                .map_err(|error| format!("bad hex at byte {}: {error}", index / 2))
        })
        .collect()
}
