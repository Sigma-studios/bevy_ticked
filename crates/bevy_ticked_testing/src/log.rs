//! Capturing what the peers log, so a test can assert that they said nothing alarming.
//!
//! Half of what this stack does when something goes wrong is `warn!`: a snapshot older than the
//! history it would roll back into, a packet that would not decode, a husk left by a rewind past
//! a spawn, a client minting a tracked id. None of those fail a test on their own — the session
//! limps on, the numbers still converge — and so every one of them shipped at least once with a
//! green suite behind it. The warning was in the output; nobody reads the output of a passing
//! test.
//!
//! This turns the output into something a test can read. A [`tracing_subscriber`] layer is
//! installed once per process, records every `warn!` and `error!`, and [`warnings_since`] returns
//! what came after a [`LogMark`] the test took.
//!
//! # Scope
//!
//! Bevy's `LogPlugin` is not used: `MinimalPlugins` has none, and installing it in every peer
//! would fail on the second (`tracing` allows one global subscriber) with an error log of its own,
//! which is a poor start for a log-hygiene check. The layer is installed with
//! [`tracing::subscriber::set_global_default`]; if something else got there first, capture is
//! silently unavailable and every assertion here passes vacuously — so a test that depends on it
//! should also check that a warning it provokes on purpose is seen.
//!
//! Records carry the thread they were emitted on and [`warnings_since`] returns those from the
//! calling thread, so tests running in parallel do not read each other's output — **for what is
//! logged from the test's own thread.** That is most of it: packet decoding, snapshot handling,
//! rollback and the observers are exclusive systems and run on the thread that called
//! `App::update`. A warning from a *parallel* system runs on a task-pool thread and cannot be
//! attributed; it is returned to whichever test asks. If a log assertion flakes under parallel
//! tests, that is why, and `--test-threads=1` settles it.

use std::sync::{Mutex, Once};
use std::thread::ThreadId;

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

/// One captured `warn!` or `error!`.
#[derive(Clone, Debug)]
pub struct LogRecord {
    pub thread: ThreadId,
    /// Whether the thread was a task-pool worker, whose records cannot be attributed to a test.
    pub pooled: bool,
    pub level: Level,
    pub target: String,
    pub message: String,
}

impl LogRecord {
    /// `target: message`, which is what the assertions print.
    pub fn line(&self) -> String {
        format!("{}: {}", self.target, self.message)
    }
}

static RECORDS: Mutex<Vec<LogRecord>> = Mutex::new(Vec::new());
static INSTALL: Once = Once::new();

/// Install the capture layer as the process's global `tracing` subscriber, once.
///
/// [`peer_app_with`](crate::peer::peer_app_with) calls this, so a test that builds its peers
/// through this crate never has to. Idempotent, and harmless if another subscriber already won.
pub fn install() {
    INSTALL.call_once(|| {
        let subscriber = tracing_subscriber::registry().with(CaptureLayer);
        // Somebody else installed a subscriber first — a `LogPlugin` in a test that also runs a
        // real app, say. Capture is then unavailable, which the module docs warn about; failing
        // here would fail every test in the process for a reason unrelated to any of them.
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
}

struct CaptureLayer;

impl<S: Subscriber> Layer<S> for CaptureLayer {
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        let metadata = event.metadata();
        // `tracing` orders levels by verbosity: TRACE > DEBUG > INFO > WARN > ERROR.
        if *metadata.level() > Level::WARN {
            return;
        }
        let mut message = MessageVisitor::default();
        event.record(&mut message);
        let thread = std::thread::current();
        let pooled = thread
            .name()
            .is_none_or(|name| name.contains("Task Pool") || name.contains("task pool"));
        let record = LogRecord {
            thread: thread.id(),
            pooled,
            level: *metadata.level(),
            target: metadata.target().to_string(),
            message: message.finish(),
        };
        if let Ok(mut records) = RECORDS.lock() {
            records.push(record);
        }
    }
}

#[derive(Default)]
struct MessageVisitor {
    message: String,
    fields: Vec<String>,
}

impl MessageVisitor {
    fn finish(self) -> String {
        if self.fields.is_empty() {
            self.message
        } else if self.message.is_empty() {
            self.fields.join(" ")
        } else {
            format!("{} {}", self.message, self.fields.join(" "))
        }
    }
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        } else {
            self.fields.push(format!("{}={value:?}", field.name()));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields.push(format!("{}={value}", field.name()));
        }
    }
}

/// A point in the log. Everything a test cares about comes after one.
#[derive(Clone, Copy, Debug)]
pub struct LogMark {
    index: usize,
}

/// Take a mark at the current end of the log.
pub fn mark() -> LogMark {
    let index = RECORDS.lock().map_or(0, |records| records.len());
    LogMark { index }
}

/// Every record since `mark` that this thread can be held responsible for: emitted on it, or on
/// a task-pool thread (see the module docs for why those are included).
pub fn records_since(mark: &LogMark) -> Vec<LogRecord> {
    let me = std::thread::current().id();
    RECORDS.lock().map_or_else(
        |_| Vec::new(),
        |records| {
            records
                .iter()
                .skip(mark.index)
                .filter(|record| record.thread == me || record.pooled)
                .cloned()
                .collect()
        },
    )
}

/// The warnings since `mark`, as `target: message` lines.
pub fn warnings_since(mark: &LogMark) -> Vec<String> {
    records_since(mark)
        .into_iter()
        .filter(|record| record.level == Level::WARN)
        .map(|record| record.line())
        .collect()
}

/// The errors since `mark`, as `target: message` lines.
pub fn errors_since(mark: &LogMark) -> Vec<String> {
    records_since(mark)
        .into_iter()
        .filter(|record| record.level == Level::ERROR)
        .map(|record| record.line())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_warning_provoked_on_purpose_is_seen_and_an_earlier_one_is_not() {
        install();
        tracing::warn!(target: "bevy_ticked_testing::log::test", "before the mark");
        let mark = mark();
        tracing::warn!(target: "bevy_ticked_testing::log::test", "after the mark");
        tracing::info!(target: "bevy_ticked_testing::log::test", "not a warning");

        let warnings = warnings_since(&mark);
        assert!(
            warnings.iter().any(|line| line.contains("after the mark")),
            "the warning after the mark was not captured: {warnings:?}"
        );
        assert!(
            !warnings.iter().any(|line| line.contains("before the mark")),
            "a warning from before the mark leaked through: {warnings:?}"
        );
        assert!(
            !warnings.iter().any(|line| line.contains("not a warning")),
            "an info line was reported as a warning"
        );
    }
}
