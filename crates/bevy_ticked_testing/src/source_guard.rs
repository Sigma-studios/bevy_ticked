//! The grep-the-source guardrail: a ticked simulation may not reach for the frame.
//!
//! A ticked system must be a pure function of the tick and its inputs. Nothing enforces that:
//! Rust has no way to say "this module may not name that type", and every system in the tick has
//! the whole `World` available if it asks for it. So this reads the source and looks for
//! substrings. Crude, but it catches the thing it exists to catch, which is the reflex reach: the
//! fastest way to make something animate is to grab `Res<Time<Virtual>>` in the system that
//! already has the entity, and by the time somebody notices it is load bearing.
//!
//! Every consumer of `bevy_ticked` had written one of these. This is that, once, with the parts
//! the copies had each got half of:
//!
//! - **Listed sources, not discovered ones.** "Runs in the tick" is a property of where a system
//!   is registered, not of where it is written, so a guard is told which files are ticked and
//!   which files under them are not, and [`SourceGuard::assert_excluded_paths_exist`] keeps the
//!   exclusions from outliving the files they excuse.
//! - **Needles spelled to match a clock *read*.** `Time<Virtual>` rather than `Time`, so a
//!   tick stamp called `LastFiredTime` does not trip it. A guardrail that cries wolf gets an
//!   exception added to it, and then it guards nothing.
//! - **[`code_only`] strips comments *and* string literals**, so a comment explaining the rule
//!   and a log message quoting it do not count.
//! - **Exceptions are data, with a reason and an optional expiry.** The debt is enumerated, the
//!   list shrinking is visible progress, and [`SourceGuard::assert_every_exception_is_still_needed`]
//!   fails when an allowance stops being used, so a stale one cannot quietly accumulate.
//! - **[`SourceGuard::assert_it_bites`]** proves the guard fails on a file that contains every
//!   needle before anything is asserted with it. A guardrail nobody has seen fail is a guardrail
//!   nobody knows works.
//!
//! # Use
//!
//! ```no_run
//! use bevy_ticked_testing::source_guard::{DEFAULT_NEEDLES, Exception, SourceGuard};
//!
//! let guard = SourceGuard::new(env!("CARGO_MANIFEST_DIR"))
//!     .sources(&["src", "examples"])
//!     .not_ticked(&["src/visuals.rs"])
//!     .ban(DEFAULT_NEEDLES)
//!     .allow(&[Exception {
//!         path: "examples/demo.rs",
//!         needle: "ButtonInput",
//!         reason: "the example reads its keyboard in Update and hands the tick an action",
//!         expires: None,
//!     }]);
//! guard.assert_it_bites();
//! guard.assert_excluded_paths_exist();
//! guard.assert_every_exception_is_still_needed();
//! guard.assert_no_exception_has_expired();
//! guard.assert_clean();
//! ```

use std::fs;
use std::path::{Path, PathBuf};

/// A substring that may not appear in ticked source, and why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Needle {
    /// The substring, spelled to match the *reach* and not merely the word.
    pub needle: &'static str,
    /// One line, printed next to every violation. Say what to do instead.
    pub reason: &'static str,
}

/// One file that is allowed one needle, with the reason and, if the reason is temporary, the
/// date by which it should have gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Exception {
    /// Relative to the manifest directory, with forward slashes: `src/lib.rs`.
    pub path: &'static str,
    /// Exactly one of the guard's needles.
    pub needle: &'static str,
    /// Why this file is allowed this reach. Written for the person who finds it in a year.
    pub reason: &'static str,
    /// `"YYYY-MM-DD"`, or `None` for an exception that is right rather than pending.
    ///
    /// [`SourceGuard::assert_no_exception_has_expired`] fails once today is past this date, so
    /// a "we will fix it in the next phase" that nobody fixed does not become permanent by
    /// default.
    pub expires: Option<&'static str>,
}

/// What a ticked simulation reaches for by reflex, and must not.
///
/// The frame clock, the keyboard, the thread's randomness, the wall clock, and rendering. Each
/// needle is spelled to match a *read* of the thing — `Time<Virtual>`, not `Time` — because the
/// point is to catch the reach without flagging the tick stamps and tick-clock reads that are
/// exactly right. `Res<Time>` is not here on purpose: `bevy_ticked` sets `Time` to the tick clock
/// for the duration of the tick, and reading it is the sanctioned way to know how long a tick is.
pub const DEFAULT_NEEDLES: &[Needle] = &[
    Needle {
        needle: "Res<Time<Virtual>",
        reason: "reads the frame clock inside a tick; use Res<Time>, which bevy_ticked sets to the tick clock",
    },
    Needle {
        needle: "Res<Time<Real>",
        reason: "reads the wall clock inside a tick; use Res<Time>, which bevy_ticked sets to the tick clock",
    },
    Needle {
        needle: "Res<Time<Fixed>",
        reason: "reads the fixed-step clock inside a tick; use Res<Time>, which bevy_ticked sets to the tick clock",
    },
    Needle {
        needle: "ResMut<Time<",
        reason: "writes a clock; only bevy_ticked's own driver may do that",
    },
    Needle {
        needle: "Time<Virtual>",
        reason: "names the frame clock; a tick advances by exactly one tick, not by the frame's delta",
    },
    Needle {
        needle: "Time<Real>",
        reason: "names the wall clock; wall-clock time is not simulation state",
    },
    Needle {
        needle: "delta_secs",
        reason: "the frame delta is not the tick length; read Res<Time>::delta() inside the tick",
    },
    Needle {
        needle: "elapsed_secs",
        reason: "elapsed frame time is not simulation state; count ticks with CurrentTick",
    },
    Needle {
        needle: "ButtonInput",
        reason: "input belongs outside the tick, captured once and handed to the tick as data",
    },
    Needle {
        needle: "rand::",
        reason: "randomness in a tick must be seeded from the tick and the world, not from the thread",
    },
    Needle {
        needle: "thread_rng",
        reason: "the thread's randomness differs on every peer; seed a generator from the tick",
    },
    Needle {
        needle: "rand::rng(",
        reason: "the thread's randomness differs on every peer; seed a generator from the tick",
    },
    Needle {
        needle: "SystemTime",
        reason: "wall-clock time is not simulation state",
    },
    Needle {
        needle: "Instant::now",
        reason: "wall-clock time is not simulation state",
    },
    Needle {
        needle: "Gizmos<",
        reason: "drawing is not simulation; draw from a frame system that reads the tick's state",
    },
    Needle {
        needle: "Sprite",
        reason: "rendering is not simulation; a headless peer has no sprites and must tick identically",
    },
    Needle {
        needle: "AssetServer",
        reason: "assets load asynchronously and differ per peer; the tick may not depend on one",
    },
    Needle {
        needle: "SECONDS_PER_TICK",
        reason: "a second source of truth for the tick length that Time<Fixed> can disagree with; read Res<Time>::delta() inside the tick",
    },
];

/// One banned substring found in one line of ticked source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    /// Relative to the manifest directory, with forward slashes.
    pub path: String,
    /// One-based, as an editor counts.
    pub line: usize,
    pub needle: &'static str,
    pub reason: &'static str,
    /// The offending line, with comments and string literals already stripped.
    pub text: String,
}

impl std::fmt::Display for Violation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{}:{}: `{}` — {}\n      {}",
            self.path,
            self.line,
            self.needle,
            self.reason,
            self.text.trim()
        )
    }
}

/// A source guard: which files are ticked, what they may not contain, and who is excused.
///
/// Built once per test file and asked its five questions; see the [module docs](self).
#[derive(Clone, Debug)]
pub struct SourceGuard {
    manifest_dir: PathBuf,
    sources: Vec<String>,
    not_ticked: Vec<String>,
    needles: Vec<Needle>,
    exceptions: Vec<Exception>,
    min_files: usize,
}

impl SourceGuard {
    /// A guard over nothing, rooted at `manifest_dir` — pass `env!("CARGO_MANIFEST_DIR")`.
    ///
    /// Starts with no needles: add [`DEFAULT_NEEDLES`] with [`ban`](Self::ban).
    pub fn new(manifest_dir: &str) -> Self {
        Self {
            manifest_dir: PathBuf::from(manifest_dir),
            sources: Vec::new(),
            not_ticked: Vec::new(),
            needles: Vec::new(),
            exceptions: Vec::new(),
            min_files: 1,
        }
    }

    /// Files or directories whose systems run inside the tick, relative to the manifest
    /// directory. Directories are recursed for `.rs` files.
    ///
    /// Listed rather than discovered, because a grep for the schedule name would miss every
    /// helper called from an exclusive system.
    pub fn sources(mut self, sources: &[&str]) -> Self {
        self.sources
            .extend(sources.iter().map(|source| source.to_string()));
        self
    }

    /// Paths inside [`sources`](Self::sources) that are *not* part of the tick, relative to the
    /// manifest directory. A file or a directory; a directory excludes everything under it.
    ///
    /// This silently widens what the guard permits, so
    /// [`assert_excluded_paths_exist`](Self::assert_excluded_paths_exist) keeps it honest.
    pub fn not_ticked(mut self, paths: &[&str]) -> Self {
        self.not_ticked
            .extend(paths.iter().map(|path| path.to_string()));
        self
    }

    /// Add needles. Usually [`DEFAULT_NEEDLES`], plus the game's own.
    pub fn ban(mut self, needles: &[Needle]) -> Self {
        self.needles.extend_from_slice(needles);
        self
    }

    /// Add exceptions. A file/needle pair listed here passes; anything else does not.
    pub fn allow(mut self, exceptions: &[Exception]) -> Self {
        self.exceptions.extend_from_slice(exceptions);
        self
    }

    /// Fail [`assert_clean`](Self::assert_clean) if fewer than `n` files were found, which is
    /// what a stale [`sources`](Self::sources) list looks like. Defaults to 1.
    pub fn min_files(mut self, n: usize) -> Self {
        self.min_files = n;
        self
    }

    /// Every `.rs` file under the sources, minus the exclusions, sorted.
    pub fn files(&self) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for source in &self.sources {
            files.extend(rust_files(&self.manifest_dir.join(source)));
        }
        files.retain(|file| {
            let relative = self.relative(file);
            !self.not_ticked.iter().any(|excluded| {
                let excluded = excluded.trim_end_matches('/');
                relative == excluded || relative.starts_with(&format!("{excluded}/"))
            })
        });
        files.sort();
        files.dedup();
        files
    }

    /// Every banned needle in every ticked line, minus the exceptions.
    pub fn violations(&self) -> Vec<Violation> {
        let mut violations = Vec::new();
        for file in self.files() {
            let source = fs::read_to_string(&file)
                .unwrap_or_else(|error| panic!("cannot read `{}`: {error}", file.display()));
            let path = self.relative(&file);
            violations.extend(self.violations_in(&path, &source));
        }
        violations
    }

    fn violations_in(&self, path: &str, source: &str) -> Vec<Violation> {
        let code = code_only(source);
        let mut violations = Vec::new();
        for (index, text) in code.lines().enumerate() {
            for needle in &self.needles {
                if !text.contains(needle.needle) || self.is_allowed(path, needle.needle) {
                    continue;
                }
                violations.push(Violation {
                    path: path.to_string(),
                    line: index + 1,
                    needle: needle.needle,
                    reason: needle.reason,
                    text: text.to_string(),
                });
            }
        }
        violations
    }

    fn is_allowed(&self, path: &str, needle: &str) -> bool {
        self.exceptions
            .iter()
            .any(|exception| exception.path == path && exception.needle == needle)
    }

    fn relative(&self, file: &Path) -> String {
        file.strip_prefix(&self.manifest_dir)
            .unwrap_or(file)
            .to_string_lossy()
            .replace('\\', "/")
    }

    /// The tick reaches for nothing it may not. Panics listing every violation as `path:line`.
    ///
    /// Also fails when fewer than [`min_files`](Self::min_files) files were found, which is what
    /// a renamed directory looks like from here: a guard over nothing passes.
    pub fn assert_clean(&self) {
        let files = self.files();
        assert!(
            files.len() >= self.min_files,
            "the source guard found {} ticked file(s) under {:?} but expected at least {}; \
             the source list has gone stale",
            files.len(),
            self.sources,
            self.min_files
        );
        let violations = self.violations();
        assert!(
            violations.is_empty(),
            "the tick simulation reaches outside the tick ({} violation(s)):\n  {}\n\n\
             Fix the reach, or add an `Exception` with a reason if it is genuinely right.",
            violations.len(),
            violations
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n  ")
        );
    }

    /// Every exception names a file that exists and still contains its needle.
    ///
    /// An exception that no longer applies is worse than no exception: it is a licence nobody is
    /// using and everybody has to reason about. The list has to shrink as reaches are fixed, and
    /// this fails if it has not.
    pub fn assert_every_exception_is_still_needed(&self) {
        for exception in &self.exceptions {
            assert!(
                self.needles
                    .iter()
                    .any(|needle| needle.needle == exception.needle),
                "the exception for `{}` in `{}` names a needle this guard does not ban",
                exception.needle,
                exception.path
            );
            let file = self.manifest_dir.join(exception.path);
            let source = fs::read_to_string(&file).unwrap_or_else(|error| {
                panic!(
                    "the exception for `{}` names `{}`, which cannot be read ({error}); \
                     delete the exception",
                    exception.needle, exception.path
                )
            });
            assert!(
                code_only(&source).contains(exception.needle),
                "`{}` no longer contains `{}` ({}); delete the exception",
                exception.path,
                exception.needle,
                exception.reason
            );
        }
    }

    /// No exception's `expires` date is in the past.
    ///
    /// "Today" is the `DATE_TODAY` environment variable when set (`YYYY-MM-DD`, for CI and for
    /// tests of this guard), else the date the process is running on, read from the system
    /// clock. That fallback means an expiry fires on whichever machine first runs the test
    /// after the date, which is the intent: the exception was a promise with a deadline.
    pub fn assert_no_exception_has_expired(&self) {
        let today = std::env::var("DATE_TODAY").unwrap_or_else(|_| today());
        self.assert_no_exception_has_expired_on(&today);
    }

    /// [`assert_no_exception_has_expired`](Self::assert_no_exception_has_expired) against a
    /// given `YYYY-MM-DD`.
    pub fn assert_no_exception_has_expired_on(&self, today: &str) {
        assert!(
            is_iso_date(today),
            "today's date must be `YYYY-MM-DD`, got `{today}`"
        );
        let mut expired = Vec::new();
        for exception in &self.exceptions {
            let Some(expires) = exception.expires else {
                continue;
            };
            assert!(
                is_iso_date(expires),
                "the exception for `{}` in `{}` expires on `{expires}`, which is not `YYYY-MM-DD`",
                exception.needle,
                exception.path
            );
            // ISO dates sort as strings.
            if expires < today {
                expired.push(format!(
                    "{}: `{}` expired on {expires} — {}",
                    exception.path, exception.needle, exception.reason
                ));
            }
        }
        assert!(
            expired.is_empty(),
            "{} exception(s) have expired (today is {today}):\n  {}\n\n\
             Fix the reach, or renew the date with a reason it is still pending.",
            expired.len(),
            expired.join("\n  ")
        );
    }

    /// Every [`not_ticked`](Self::not_ticked) path still exists.
    ///
    /// A stale entry there is a hole: it excuses whatever is created at that path next.
    pub fn assert_excluded_paths_exist(&self) {
        for path in &self.not_ticked {
            assert!(
                self.manifest_dir.join(path).exists(),
                "`{path}` is excluded from the source guard but no longer exists"
            );
        }
    }

    /// The guard fails on a file that contains every banned needle, and passes on one that only
    /// mentions them in comments and strings.
    ///
    /// Writes two files under [`std::env::temp_dir`], runs a guard with these needles and no
    /// exceptions over them, and removes them. Call it before anything is asserted with the
    /// guard: checking the check is the only evidence a passing guard is worth having.
    pub fn assert_it_bites(&self) {
        assert!(
            !self.needles.is_empty(),
            "the source guard bans nothing; add DEFAULT_NEEDLES"
        );
        let scratch = Scratch::new("source_guard_bites");
        let code = self
            .needles
            .iter()
            .map(|needle| format!("let _ = {};", needle.needle))
            .collect::<Vec<_>>()
            .join("\n");
        let prose = self
            .needles
            .iter()
            .map(|needle| format!("// {}\nlet _ = \"{}\";", needle.needle, needle.needle))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(scratch.dir.join("code.rs"), code).expect("the scratch dir is writable");
        fs::write(scratch.dir.join("prose.rs"), prose).expect("the scratch dir is writable");

        let guard = SourceGuard::new(&scratch.dir.to_string_lossy())
            .sources(&["code.rs"])
            .ban(&self.needles);
        let violations = guard.violations();
        for (index, needle) in self.needles.iter().enumerate() {
            assert!(
                violations
                    .iter()
                    .any(|violation| violation.needle == needle.needle
                        && violation.line == index + 1),
                "the source guard did not bite on `{}` at code.rs:{}; it guards nothing",
                needle.needle,
                index + 1
            );
        }

        let guard = SourceGuard::new(&scratch.dir.to_string_lossy())
            .sources(&["prose.rs"])
            .ban(&self.needles);
        let violations = guard.violations();
        assert!(
            violations.is_empty(),
            "a comment or string mentioning a needle tripped the source guard:\n  {}",
            violations
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n  ")
        );
    }
}

/// A directory under [`std::env::temp_dir`], removed on drop.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(purpose: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "bevy_ticked_{purpose}_{}_{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("the temp dir is writable");
        Self { dir }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Every `.rs` file at or under `root`, unsorted. A file is returned as itself; a missing path
/// is empty.
pub fn rust_files(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if root.is_file() {
        if root.extension().is_some_and(|extension| extension == "rs") {
            found.push(root.to_path_buf());
        }
        return found;
    }
    let Ok(entries) = fs::read_dir(root) else {
        return found;
    };
    for entry in entries.flatten() {
        found.extend(rust_files(&entry.path()));
    }
    found
}

/// `source` with `//` comments, `/* */` comments and string literals removed, and every line
/// still on its own line — so a line number in the result is a line number in the file.
///
/// Deliberately simple. It nests block comments, honours `\"` inside a string and knows that
/// `'"'` is a char and not the start of one, but it does not understand raw strings with hashes
/// (`r#"..."#` containing a `"`). A guardrail that is wrong in the paranoid direction is fine;
/// this errs by keeping too much, never by dropping code.
pub fn code_only(source: &str) -> String {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum State {
        Code,
        LineComment,
        BlockComment(u32),
        Str,
    }

    let mut out = String::with_capacity(source.len());
    let mut state = State::Code;
    let characters: Vec<char> = source.chars().collect();
    let mut index = 0;
    while index < characters.len() {
        let character = characters[index];
        let next = characters.get(index + 1).copied();
        match state {
            State::Code => match (character, next) {
                ('/', Some('/')) => {
                    state = State::LineComment;
                    index += 2;
                }
                ('/', Some('*')) => {
                    state = State::BlockComment(1);
                    index += 2;
                }
                ('\'', Some('"')) if characters.get(index + 2) == Some(&'\'') => {
                    // The char literal `'"'`: keep it, and do not open a string on it.
                    out.push_str("'\"'");
                    index += 3;
                }
                ('"', _) => {
                    state = State::Str;
                    index += 1;
                }
                _ => {
                    out.push(character);
                    index += 1;
                }
            },
            State::LineComment => {
                if character == '\n' {
                    out.push('\n');
                    state = State::Code;
                }
                index += 1;
            }
            State::BlockComment(depth) => match (character, next) {
                ('/', Some('*')) => {
                    state = State::BlockComment(depth + 1);
                    index += 2;
                }
                ('*', Some('/')) => {
                    state = if depth == 1 {
                        State::Code
                    } else {
                        State::BlockComment(depth - 1)
                    };
                    index += 2;
                }
                _ => {
                    if character == '\n' {
                        out.push('\n');
                    }
                    index += 1;
                }
            },
            State::Str => match (character, next) {
                ('\\', Some(_)) => {
                    if next == Some('\n') {
                        out.push('\n');
                    }
                    index += 2;
                }
                ('"', _) => {
                    state = State::Code;
                    index += 1;
                }
                _ => {
                    if character == '\n' {
                        out.push('\n');
                    }
                    index += 1;
                }
            },
        }
    }
    out
}

fn is_iso_date(date: &str) -> bool {
    let bytes = date.as_bytes();
    bytes.len() == 10
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            4 | 7 => *byte == b'-',
            _ => byte.is_ascii_digit(),
        })
}

/// Today as `YYYY-MM-DD` in UTC, from the system clock.
///
/// The one wall-clock read in this crate's guardrails, and the reason the guardrails do not run
/// under their own needles.
fn today() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the clock is after 1970")
        .as_secs();
    let (year, month, day) = civil_from_days((seconds / 86_400) as i64);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 to a proleptic Gregorian date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = (z - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_index + 2) / 5 + 1) as u32;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_convert() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(20_707), (2026, 9, 11));
        assert!(is_iso_date("2026-12-31"));
        assert!(!is_iso_date("2026-12-3"));
        assert!(!is_iso_date("2026/12/31"));
    }
}
