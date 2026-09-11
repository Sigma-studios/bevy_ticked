//! Do real processes, over a real data channel, end up with the same world?
//!
//! Everything else in this directory runs peers as `App`s in one process with packets handed
//! between them by a `Vec`. That is the right way to test the simulation. It cannot fail the
//! way a shipped session fails: no signalling handshake, no ICE, no serialization across an
//! address space, no second clock. These tests run the real thing — a signalling server on a
//! throwaway port and two or three `netpeer` processes (`examples/netpeer.rs`) that host, join,
//! play a scripted session and write a per-tick checksum log — and compare the logs tick for
//! tick.
//!
//! Ignored by default: they need the `netpeer` example built and a few seconds of wall clock
//! each. `cargo test -p bevy_ticked_networking_ensemble --test webrtc_multiprocess -- --ignored
//! --test-threads=1`, which is what the `multiprocess` CI job runs. A peer that exits 3 (the
//! transport never connected) is INCONCLUSIVE: the session is retried once, then the test fails
//! naming the transport rather than the game.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use bevy_ensemble_webrtc::server::test_support::SignallingServer;

/// How far into the session to run. Two seconds of settling, then the script for the rest.
const TARGET_TICK: u64 = 400;
/// A peer that has not finished by now has hung, and a hung test is worse than a failed one.
const PEER_TIMEOUT: Duration = Duration::from_secs(60);
/// Below this, the peers overlapped so little that agreeing proves nothing.
const MINIMUM_COMPARED_TICKS: usize = 120;
/// A client must have applied its first snapshot this long after starting.
const JOIN_DEADLINE: Duration = Duration::from_secs(15);

fn one_session_at_a_time() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[test]
#[ignore = "real WebRTC between processes; run with --ignored, as the multiprocess CI job does"]
fn two_processes_agree_about_the_session_they_played() {
    let session = play_a_session("two-peers", 1);
    assert_peers_agree(&session);
}

#[test]
#[ignore = "real WebRTC between processes; run with --ignored, as the multiprocess CI job does"]
fn three_processes_agree_about_the_session_they_played() {
    let session = play_a_session("three-peers", 2);
    assert_peers_agree(&session);
}

#[test]
#[ignore = "real WebRTC between processes; run with --ignored, as the multiprocess CI job does"]
fn a_client_joining_a_real_session_sees_the_hosts_world_within_the_deadline() {
    let session = play_a_session("join-deadline", 1);
    let client = &session.peers[1];
    let started = client
        .session_start()
        .unwrap_or_else(|| panic!("{} never logged LOG_SESSION_START", client.name));
    let took = started - client.started_at;
    println!(
        "{} applied its first snapshot {took:?} after starting",
        client.name
    );
    assert!(
        took <= JOIN_DEADLINE,
        "{} took {took:?} to see the host's world, over the {JOIN_DEADLINE:?} deadline",
        client.name
    );
}

#[test]
#[ignore = "real WebRTC between processes; run with --ignored, as the multiprocess CI job does"]
fn no_peer_logs_a_warning_after_the_session_starts() {
    let session = play_a_session("warnings", 1);
    for peer in &session.peers {
        let noise = peer.warnings_after_session_start();
        assert!(
            noise.is_empty(),
            "{} logged {} warnings or errors after the session started:\n{}",
            peer.name,
            noise.len(),
            noise.join("\n")
        );
    }
}

struct Session {
    peers: Vec<Peer>,
}

/// A host, `client_count` clients, and the signalling server they find each other through.
fn play_a_session(name: &str, client_count: u32) -> Session {
    let _one_at_a_time = one_session_at_a_time();
    for attempt in 1..=2 {
        match try_a_session(name, client_count) {
            Ok(session) => return session,
            Err(inconclusive) if attempt == 1 => {
                eprintln!("{inconclusive}; retrying once");
            }
            Err(inconclusive) => panic!("{inconclusive}"),
        }
    }
    unreachable!()
}

fn try_a_session(name: &str, client_count: u32) -> Result<Session, String> {
    let output_directory = output_root().join(name);
    let _ = fs::remove_dir_all(&output_directory);
    fs::create_dir_all(&output_directory).expect("could not create the output directory");

    let server = SignallingServer::start();
    let mut peers = vec![Peer::spawn(
        "host",
        &server,
        &output_directory,
        &["--role", "host", "--walk", "--linger", "6000"],
    )];
    for index in 0..client_count {
        let name = format!("client-{index}");
        let args = [
            "--role",
            "client",
            "--index",
            &index.to_string(),
            "--walk",
            "--fire",
            "--linger",
            "2000",
        ];
        peers.push(Peer::spawn(&name, &server, &output_directory, &args));
    }
    let deadline = Instant::now() + PEER_TIMEOUT;
    let mut inconclusive = None;
    for peer in &mut peers {
        if let Err(reason) = peer.wait_until(deadline) {
            inconclusive.get_or_insert(reason);
        }
    }
    match inconclusive {
        Some(reason) => Err(reason),
        None => Ok(Session { peers }),
    }
}

fn output_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/webrtc-multiprocess")
}

fn netpeer_binary() -> PathBuf {
    // target/debug/deps/<this test> -> target/debug/examples/netpeer
    let this = std::env::current_exe().expect("this test has a path");
    let candidate = this
        .parent()
        .and_then(Path::parent)
        .map(|debug| debug.join("examples").join("netpeer"))
        .expect("the test binary sits under target/<profile>/deps");
    assert!(
        candidate.exists(),
        "no netpeer example at {}: build it with `cargo build -p bevy_ticked_networking_ensemble \
         --example netpeer`",
        candidate.display()
    );
    candidate
}

fn assert_peers_agree(session: &Session) {
    let host = &session.peers[0];
    let host_samples = host.samples();
    assert!(
        !host_samples.is_empty(),
        "{} wrote no checksums at all",
        host.name
    );
    for peer in &session.peers[1..] {
        let samples = peer.samples();
        assert!(
            !samples.is_empty(),
            "{} wrote no checksums at all",
            peer.name
        );
        let compared = compare(&host_samples, &samples);
        assert!(
            compared.overlap >= MINIMUM_COMPARED_TICKS,
            "{} and {} only overlapped on {} ticks, too few to conclude anything",
            host.name,
            peer.name,
            compared.overlap
        );
        assert!(
            compared.world_changed,
            "the world never changed over the {} ticks {} and {} shared: the script did not \
             reach the simulation",
            compared.overlap, host.name, peer.name
        );
        if let Some(divergence) = compared.first_divergence {
            panic!(
                "{} and {} disagree from tick {} onwards: {:016x} vs {:016x}, differing in {}\n\
                 (logs under {})",
                host.name,
                peer.name,
                divergence.tick,
                divergence.left,
                divergence.right,
                divergence.sections.join(", "),
                output_root().display()
            );
        }
        println!(
            "{} and {} agree on {} shared ticks",
            host.name, peer.name, compared.overlap
        );
    }
}

struct Peer {
    name: String,
    child: Option<Child>,
    checksums: PathBuf,
    log: PathBuf,
    started_at: Instant,
}

impl Peer {
    fn spawn(name: &str, server: &SignallingServer, directory: &Path, arguments: &[&str]) -> Self {
        let checksums = directory.join(format!("{name}.checksums"));
        let log = directory.join(format!("{name}.log"));
        let file = File::create(&log).expect("could not create the peer log");
        let errors = file.try_clone().expect("could not share the peer log");
        let child = Command::new(netpeer_binary())
            .args(arguments)
            .args(["--ticks", &TARGET_TICK.to_string()])
            .args(["--out", &checksums.to_string_lossy()])
            .args(["--timeout", &PEER_TIMEOUT.as_secs().to_string()])
            .env("SIGNALLING_SERVER_URL", server.ws_url())
            .env(
                "RUST_LOG",
                std::env::var("NETPEER_LOG").unwrap_or_else(|_| "info".into()),
            )
            .stdout(Stdio::from(file))
            .stderr(Stdio::from(errors))
            .spawn()
            .expect("could not start a peer");
        Self {
            name: name.to_string(),
            child: Some(child),
            checksums,
            log,
            started_at: Instant::now(),
        }
    }

    /// `Err` when the peer reported the transport never connected (exit 3).
    fn wait_until(&mut self, deadline: Instant) -> Result<(), String> {
        let child = self.child.as_mut().expect("peer is still running");
        loop {
            match child.try_wait().expect("could not poll a peer") {
                Some(status) if status.code() == Some(3) => {
                    return Err(format!(
                        "INCONCLUSIVE: {}'s transport never connected (see {})",
                        self.name,
                        self.log.display()
                    ));
                }
                Some(status) => {
                    assert!(
                        status.success(),
                        "{} exited with {status} — see {}",
                        self.name,
                        self.log.display()
                    );
                    return Ok(());
                }
                None if Instant::now() >= deadline => {
                    let _ = child.kill();
                    panic!(
                        "{} was still running after {}s — see {}",
                        self.name,
                        PEER_TIMEOUT.as_secs(),
                        self.log.display()
                    );
                }
                None => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    }

    fn samples(&self) -> Vec<Sample> {
        let Ok(file) = File::open(&self.checksums) else {
            return Vec::new();
        };
        let mut by_tick: BTreeMap<u64, Sample> = BTreeMap::new();
        for sample in BufReader::new(file)
            .lines()
            .map_while(Result::ok)
            .filter_map(|line| Sample::parse(&line))
        {
            by_tick.insert(sample.tick, sample);
        }
        by_tick.into_values().collect()
    }

    fn log_lines(&self) -> Vec<String> {
        fs::read_to_string(&self.log)
            .map(|text| text.lines().map(strip_ansi).collect())
            .unwrap_or_default()
    }

    /// When the peer logged `LOG_SESSION_START`, from the log file's modification clock.
    fn session_start(&self) -> Option<Instant> {
        let lines = self.log_lines();
        let index = lines
            .iter()
            .position(|line| line.contains("LOG_SESSION_START"))?;
        // Bevy's log lines carry a wall-clock timestamp; the file's own clock is what we can
        // compare with `started_at` without parsing it: the line's position in a log written as
        // it goes bounds the moment. Read the timestamp when present.
        let line = &lines[index];
        let stamp = line.split_whitespace().next()?;
        let when = humantime_like(stamp)?;
        let now_stamp = std::time::SystemTime::now();
        let elapsed_since = now_stamp.duration_since(when).ok()?;
        Instant::now().checked_sub(elapsed_since)
    }

    /// WARN and ERROR lines after the session started, from this crate's stack, not the
    /// transport's ICE chatter.
    fn warnings_after_session_start(&self) -> Vec<String> {
        let lines = self.log_lines();
        let Some(start) = lines
            .iter()
            .position(|line| line.contains("LOG_SESSION_START"))
        else {
            return vec![format!("{} never logged LOG_SESSION_START", self.name)];
        };
        lines[start + 1..]
            .iter()
            .filter(|line| line.contains(" WARN ") || line.contains(" ERROR "))
            .filter(|line| !line.contains("webrtc") && !line.contains("bevy_ensemble_sockets"))
            .filter(|line| !line.contains("left the lobby"))
            .cloned()
            .collect()
    }
}

/// Bevy's log plugin colours the level; the colour codes are not part of any word.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// Parse an RFC 3339 timestamp the way Bevy's log plugin writes it (`2026-09-11T18:20:31.123456Z`).
fn humantime_like(stamp: &str) -> Option<std::time::SystemTime> {
    let (date, time) = stamp.split_once('T')?;
    let time = time.trim_end_matches('Z');
    let mut date = date.split('-');
    let (year, month, day): (i64, u32, u32) = (
        date.next()?.parse().ok()?,
        date.next()?.parse().ok()?,
        date.next()?.parse().ok()?,
    );
    let mut time = time.split(':');
    let (hour, minute, second): (u64, u64, f64) = (
        time.next()?.parse().ok()?,
        time.next()?.parse().ok()?,
        time.next()?.parse().ok()?,
    );
    // Days from the civil date (Howard Hinnant's algorithm), then seconds.
    let (y, m) = if month <= 2 {
        (year - 1, month + 12)
    } else {
        (year, month)
    };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m as i64 - 3) + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days as f64 * 86_400.0 + hour as f64 * 3600.0 + minute as f64 * 60.0 + second;
    std::time::UNIX_EPOCH.checked_add(Duration::from_secs_f64(secs))
}

impl Drop for Peer {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take()
            && matches!(child.try_wait(), Ok(None))
        {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Sample {
    tick: u64,
    checksum: u64,
    sections: [u64; 3],
}

const SECTION_NAMES: [&str; 3] = ["bodies", "positions", "velocities"];

impl Sample {
    fn parse(line: &str) -> Option<Self> {
        let mut fields = line.split_whitespace();
        let tick = fields.next()?.parse().ok()?;
        let checksum = u64::from_str_radix(fields.next()?, 16).ok()?;
        let mut sections = [0; 3];
        for section in &mut sections {
            *section = fields.next()?.parse().ok()?;
        }
        Some(Self {
            tick,
            checksum,
            sections,
        })
    }
}

struct Divergence {
    tick: u64,
    left: u64,
    right: u64,
    sections: Vec<&'static str>,
}

struct Comparison {
    overlap: usize,
    world_changed: bool,
    first_divergence: Option<Divergence>,
}

fn compare(left: &[Sample], right: &[Sample]) -> Comparison {
    let mut overlap = 0;
    let mut first_divergence = None;
    let mut positions_seen = Vec::new();
    for left_sample in left {
        let Some(right_sample) = right.iter().find(|sample| sample.tick == left_sample.tick) else {
            continue;
        };
        overlap += 1;
        positions_seen.push(left_sample.sections[1]);
        if left_sample.checksum != right_sample.checksum && first_divergence.is_none() {
            let sections = SECTION_NAMES
                .iter()
                .enumerate()
                .filter(|(index, _)| left_sample.sections[*index] != right_sample.sections[*index])
                .map(|(_, name)| *name)
                .collect();
            first_divergence = Some(Divergence {
                tick: left_sample.tick,
                left: left_sample.checksum,
                right: right_sample.checksum,
                sections,
            });
        }
    }
    positions_seen.dedup();
    Comparison {
        overlap,
        world_changed: positions_seen.len() > 1,
        first_divergence,
    }
}
