//! A tick, and a number of ticks.
//!
//! Both used to be a bare `u64`, and they are not the same kind of thing. A [`Tick`] is a moment
//! on the simulation's clock — when a snapshot was taken, when an input is for, when somebody
//! died. [`Ticks`] is an amount of the clock — a fuse, a window, how far a replay reaches. The
//! arithmetic between them is the arithmetic of moments and spans: a moment and a span make a
//! moment, two moments make the span between them, and two moments do not add. A `u64` allowed all
//! of it, and the names (`died_at`, `until`, `cooldown`, `life`, `age`) were all that said which
//! was which.
//!
//! Neither costs anything: each is a `u64`, `#[repr(transparent)]`, and serializes as one, so a
//! snapshot, a replay file and every hash over them are byte for byte what they were.

use std::fmt;
use std::ops::{Add, AddAssign, Div, Mul, Rem, Sub, SubAssign};

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

/// A moment on the simulation clock: the tick numbered `.0`, counting from the start of a session.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    Reflect,
)]
#[repr(transparent)]
#[serde(transparent)]
pub struct Tick(pub u64);

/// An amount of simulation time: `.0` ticks.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    Reflect,
)]
#[repr(transparent)]
#[serde(transparent)]
pub struct Ticks(pub u64);

impl Tick {
    /// The first tick of a session.
    pub const ZERO: Tick = Tick(0);
    /// A moment after every other, for "never" where a deadline is wanted.
    pub const MAX: Tick = Tick(u64::MAX);

    /// The tick after this one.
    #[inline]
    pub const fn next(self) -> Tick {
        Tick(self.0 + 1)
    }

    /// The tick before this one, or this one if it is the first.
    #[inline]
    pub const fn prev(self) -> Tick {
        Tick(self.0.saturating_sub(1))
    }

    /// How long after `earlier` this is, or nothing if it is not after it.
    #[inline]
    pub const fn since(self, earlier: Tick) -> Ticks {
        Ticks(self.0.saturating_sub(earlier.0))
    }

    /// How far this is from `other`, signed: positive if later, negative if earlier. For the
    /// measurements that can go either way — how early or late an input arrived.
    #[inline]
    pub const fn offset_from(self, other: Tick) -> i64 {
        self.0 as i64 - other.0 as i64
    }

    /// `self + by`, stopping at [`Tick::MAX`] rather than overflowing.
    #[inline]
    pub const fn saturating_add(self, by: Ticks) -> Tick {
        Tick(self.0.saturating_add(by.0))
    }

    /// `self - by`, stopping at [`Tick::ZERO`] rather than underflowing.
    #[inline]
    pub const fn saturating_sub(self, by: Ticks) -> Tick {
        Tick(self.0.saturating_sub(by.0))
    }

    /// `self - by`, or `None` if that would be before the first tick.
    #[inline]
    pub const fn checked_sub(self, by: Ticks) -> Option<Tick> {
        match self.0.checked_sub(by.0) {
            Some(tick) => Some(Tick(tick)),
            None => None,
        }
    }

    /// Every tick from this one to `last`, both included, in order. Empty if `last` is earlier.
    #[inline]
    pub fn through(self, last: Tick) -> impl DoubleEndedIterator<Item = Tick> {
        (self.0..=last.0).map(Tick)
    }

    /// Whether this tick falls on a multiple of `period`: every `period` ticks from the first.
    #[inline]
    pub const fn is_multiple_of(self, period: Ticks) -> bool {
        self.0.is_multiple_of(period.0)
    }
}

impl Ticks {
    /// No time at all.
    pub const ZERO: Ticks = Ticks(0);
    /// One tick.
    pub const ONE: Ticks = Ticks(1);

    /// Whether this is no time at all.
    #[inline]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// `self - other`, or nothing if `other` is longer.
    #[inline]
    pub const fn saturating_sub(self, other: Ticks) -> Ticks {
        Ticks(self.0.saturating_sub(other.0))
    }

    /// `self + other`, stopping at `u64::MAX` ticks.
    #[inline]
    pub const fn saturating_add(self, other: Ticks) -> Ticks {
        Ticks(self.0.saturating_add(other.0))
    }

    /// This long at a tick of `timestep`.
    #[inline]
    pub fn duration(self, timestep: std::time::Duration) -> std::time::Duration {
        timestep.saturating_mul(u32::try_from(self.0).unwrap_or(u32::MAX))
    }
}

impl Add<Ticks> for Tick {
    type Output = Tick;
    #[inline]
    fn add(self, by: Ticks) -> Tick {
        Tick(self.0 + by.0)
    }
}

impl AddAssign<Ticks> for Tick {
    #[inline]
    fn add_assign(&mut self, by: Ticks) {
        self.0 += by.0;
    }
}

impl Sub<Ticks> for Tick {
    type Output = Tick;
    #[inline]
    fn sub(self, by: Ticks) -> Tick {
        Tick(self.0 - by.0)
    }
}

impl SubAssign<Ticks> for Tick {
    #[inline]
    fn sub_assign(&mut self, by: Ticks) {
        self.0 -= by.0;
    }
}

/// The span between two moments. Panics, as `u64` subtraction does, if `earlier` is later; use
/// [`Tick::since`] where it may be.
impl Sub<Tick> for Tick {
    type Output = Ticks;
    #[inline]
    fn sub(self, earlier: Tick) -> Ticks {
        Ticks(self.0 - earlier.0)
    }
}

impl Add for Ticks {
    type Output = Ticks;
    #[inline]
    fn add(self, other: Ticks) -> Ticks {
        Ticks(self.0 + other.0)
    }
}

impl AddAssign for Ticks {
    #[inline]
    fn add_assign(&mut self, other: Ticks) {
        self.0 += other.0;
    }
}

impl Sub for Ticks {
    type Output = Ticks;
    #[inline]
    fn sub(self, other: Ticks) -> Ticks {
        Ticks(self.0 - other.0)
    }
}

impl SubAssign for Ticks {
    #[inline]
    fn sub_assign(&mut self, other: Ticks) {
        self.0 -= other.0;
    }
}

impl Mul<u64> for Ticks {
    type Output = Ticks;
    #[inline]
    fn mul(self, times: u64) -> Ticks {
        Ticks(self.0 * times)
    }
}

impl Div<u64> for Ticks {
    type Output = Ticks;
    #[inline]
    fn div(self, parts: u64) -> Ticks {
        Ticks(self.0 / parts)
    }
}

/// How many whole `other`s fit in this.
impl Div for Ticks {
    type Output = u64;
    #[inline]
    fn div(self, other: Ticks) -> u64 {
        self.0 / other.0
    }
}

impl Rem for Ticks {
    type Output = Ticks;
    #[inline]
    fn rem(self, other: Ticks) -> Ticks {
        Ticks(self.0 % other.0)
    }
}

/// The bare number, so a message reads as it did: `"tick {tick}"`, `"{span} ticks"`.
impl fmt::Display for Tick {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// The bare number. See [`Tick`]'s.
impl fmt::Display for Ticks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moments_and_spans_combine_as_moments_and_spans() {
        let died = Tick(100);
        let respawn = Ticks(64);
        assert_eq!(died + respawn, Tick(164));
        assert_eq!(Tick(164) - died, respawn);
        assert_eq!(Tick(90).since(died), Ticks::ZERO, "not after it is no time");
        assert_eq!(died.saturating_sub(Ticks(500)), Tick::ZERO);
        assert_eq!(died.checked_sub(Ticks(101)), None);
        assert_eq!(respawn * 2 / 4, Ticks(32));
        assert_eq!(Ticks(130) / respawn, 2);
    }

    #[test]
    fn they_are_the_size_of_what_they_replace() {
        // The wire half of "they cost nothing" is checked where snapshots are encoded, in
        // `bevy_ticked_networking`.
        assert_eq!(std::mem::size_of::<Tick>(), std::mem::size_of::<u64>());
        assert_eq!(std::mem::size_of::<Ticks>(), std::mem::size_of::<u64>());
        assert_eq!(
            std::mem::size_of::<Option<Tick>>(),
            std::mem::size_of::<Option<u64>>()
        );
    }
}
